//! Hot-reload of the underlying data (ביקורת #6). Otzaria can swap `seforim.db`
//! and rebuild the Tantivy index while the MCP is running. A `Library` holds the
//! current `Snapshot` (DB + index) behind an `RwLock`; a background task polls a
//! content fingerprint (mtime+size of the DB and both index meta files) and, once
//! a change has settled (debounce), rebuilds and swaps the snapshot as a valid
//! PAIR — a new DB is never served against a stale index. Tools always read the
//! current snapshot, so an in-flight swap never yields mixed state.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;

use crate::admission::Gate;
use crate::db::Db;
use crate::paths::Paths;
use crate::search::Search;
use tokio_util::sync::CancellationToken;

/// A consistent DB + index pair, tagged with the fingerprint it was built from.
pub struct Snapshot {
    pub db: Arc<Db>,
    /// Cached while opening, so response formatting never locks SQLite.
    pub schema_version: Option<i64>,
    pub db_schema_version: Option<i64>,
    pub search: Option<Arc<Search>>,
    /// data_version reported to clients (identical to `Db::data_version`).
    pub fingerprint: String,
    /// Why full-text search is unavailable, when it is.
    pub search_error: Option<String>,
}

pub struct Library {
    paths: Paths,
    inner: RwLock<Arc<Snapshot>>,
    reads: Gate,
    searches: Gate,
}

impl Library {
    pub fn open(paths: Paths) -> Result<Arc<Library>> {
        let snapshot = Self::build(&paths)?;
        Ok(Arc::new(Library {
            paths,
            inner: RwLock::new(Arc::new(snapshot)),
            reads: Gate::default(),
            searches: Gate::default(),
        }))
    }

    /// Cheap (Arc clone) handle to the current snapshot.
    pub fn current(&self) -> Arc<Snapshot> {
        Arc::clone(&self.inner.read().unwrap())
    }

    pub fn read_admission(&self) -> &Gate {
        &self.reads
    }

    pub fn search_admission(&self) -> &Gate {
        &self.searches
    }

    pub fn shutdown(&self) {
        self.reads.close();
        self.searches.close();
    }

    fn build(paths: &Paths) -> Result<Snapshot> {
        let fingerprint = fingerprint(paths);
        let db = Arc::new(Db::open(paths)?);
        let schema_version = db.schema_version();
        let db_schema_version = db.db_schema_version();
        let (search, search_error) = match open_search(paths) {
            Ok(s) => (s, None),
            Err(e) => (None, Some(e)),
        };
        Ok(Snapshot {
            db,
            schema_version,
            db_schema_version,
            search,
            fingerprint,
            search_error,
        })
    }

    /// Background watcher: reload when the on-disk fingerprint changes and settles.
    pub async fn watch(self: Arc<Self>) {
        const POLL: Duration = Duration::from_secs(5);
        const DEBOUNCE: Duration = Duration::from_secs(2);
        loop {
            tokio::time::sleep(POLL).await;
            let current_fp = self.current().fingerprint.clone();
            let fp = fingerprint(&self.paths);
            if fp == current_fp {
                continue;
            }
            // Change detected — wait for writes to settle, then confirm stability.
            tokio::time::sleep(DEBOUNCE).await;
            if fingerprint(&self.paths) != fp {
                continue; // still changing; re-evaluate next tick
            }
            // A large index open must neither block a Tokio runtime thread nor
            // overlap another search/reload. Take the same admission lane used by
            // search handlers, and publish while that worker still owns its permit.
            let admitted = match self.searches.acquire(CancellationToken::new()).await {
                Ok(admitted) => admitted,
                Err(crate::admission::Error::Closed) => return,
                Err(e) => {
                    eprintln!("otzaria-mcp: reload postponed: {e}");
                    continue;
                }
            };
            let library = self.clone();
            let result = admitted
                .run(move || -> Result<Option<String>> {
                    // Admission may have waited behind queries while files changed again.
                    if fingerprint(&library.paths) != fp {
                        return Ok(None);
                    }
                    let snapshot = Self::build(&library.paths)?;
                    let new_fp = snapshot.fingerprint.clone();
                    let retired = {
                        let mut current = library.inner.write().unwrap();
                        std::mem::replace(&mut *current, Arc::new(snapshot))
                    };
                    // Closing the last DB/index handle can block. Keep that work
                    // on this worker, outside the short snapshot publication lock.
                    drop(retired);
                    Ok(Some(new_fp))
                })
                .await;
            match result {
                Ok(Ok(Some(new_fp))) => {
                    eprintln!("otzaria-mcp: library changed → reloaded (data_version={new_fp})")
                }
                Ok(Ok(None)) => {} // Changed again; retry the debounce next poll.
                Ok(Err(e)) => {
                    eprintln!("otzaria-mcp: reload failed, keeping current snapshot: {e:#}")
                }
                Err(crate::admission::Error::Closed) => return,
                Err(e) => eprintln!("otzaria-mcp: reload postponed: {e}"),
            }
        }
    }
}

fn open_search(paths: &Paths) -> Result<Option<Arc<Search>>, String> {
    match &paths.index {
        Some(index_dir) => match Search::open(
            index_dir,
            paths.lexical_db.as_deref(),
            &paths.db,
            paths.dictionaries_dir.as_deref(),
        ) {
            Ok(s) => {
                eprintln!(
                    "otzaria-mcp: opened index ({} docs, schema {:?}, engine {})",
                    s.doc_count(),
                    s.info.found_schema_version,
                    s.info.engine_version
                );
                Ok(Some(Arc::new(s)))
            }
            Err(e) => {
                eprintln!("otzaria-mcp: WARNING full-text search disabled: {e:#}");
                Err(format!("{e:#}"))
            }
        },
        None => {
            eprintln!("otzaria-mcp: no Tantivy index found; full_text_search disabled");
            Err("no Tantivy index was found (pass --index or --data-dir)".to_string())
        }
    }
}

/// Snapshot fingerprint: hash of (len, mtime) over the DB + both index meta files.
pub fn fingerprint(paths: &Paths) -> String {
    let mut files: Vec<PathBuf> = vec![paths.db.clone()];
    if let Some(idx) = &paths.index {
        files.push(idx.join("meta.json"));
        files.push(idx.join("otzaria_index_meta.json"));
    }
    hash_paths(&files)
}

/// Hash a set of files by path + length + mtime. Shared with `Db::data_version`
/// so the watcher's fingerprint and the value reported to clients never diverge.
pub fn hash_paths(files: &[PathBuf]) -> String {
    let mut hasher = DefaultHasher::new();
    for path in files {
        path.to_string_lossy().hash(&mut hasher);
        if let Ok(meta) = std::fs::metadata(path) {
            meta.len().hash(&mut hasher);
            if let Ok(modified) = meta.modified() {
                if let Ok(dur) = modified.duration_since(std::time::UNIX_EPOCH) {
                    dur.as_nanos().hash(&mut hasher);
                }
            }
        }
    }
    format!("{:016x}", hasher.finish())
}
