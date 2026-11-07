//! Hot-reload of the underlying data (ביקורת #6). Otzaria can swap `seforim.db`
//! and rebuild the Tantivy index while the MCP is running. A `Library` holds the
//! current `Snapshot` (DB + index) behind an `RwLock`; a background task polls a
//! file fingerprint and, once a change has settled, rebuilds and swaps the handles.
//! This is not a transaction across the DB and index: SQLite remains WAL-aware,
//! and the engine's line source is process-wide. The engine reports mismatched
//! live rows as `stale` and suppresses highlights instead of presenting stale text
//! as indexed text. New requests capture the published snapshot in their worker.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;

use crate::admission::Gate;
use crate::db::Db;
use crate::paths::Paths;
use crate::search::Search;

/// Open DB + index handles tagged with the files observed while they were built.
pub struct Snapshot {
    pub db: Arc<Db>,
    /// Versions observed at open time (for startup logging). Responses read fresh values.
    pub schema_version: Option<i64>,
    pub db_schema_version: Option<i64>,
    pub search: Option<Arc<Search>>,
    /// data_version reported to clients. A file-change hint, not a transaction id.
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
        let before = fingerprint(paths);
        let db = Arc::new(Db::open(paths)?);
        let schema_version = db.schema_version();
        let db_schema_version = db.db_schema_version();
        let (search, search_error) = match open_search(paths) {
            Ok(s) => (s, None),
            Err(e) => (None, Some(e)),
        };
        let after = fingerprint(paths);
        anyhow::ensure!(
            before == after,
            "library files changed while opening; retry after the update settles"
        );
        Ok(Snapshot {
            db,
            schema_version,
            db_schema_version,
            search,
            fingerprint: after,
            search_error,
        })
    }

    /// Reopen a changed generation through the reserved search maintenance lane.
    /// The watcher debounces first; explicit callers can use this after finishing
    /// a known update. A change while queued or opening is never published.
    pub async fn refresh(self: &Arc<Self>) -> Result<bool> {
        self.refresh_observed(fingerprint(&self.paths)).await
    }

    async fn refresh_observed(self: &Arc<Self>, observed: String) -> Result<bool> {
        if self.current().fingerprint == observed {
            return Ok(false);
        }
        let admitted = self.searches.acquire_maintenance().await?;
        let library = self.clone();
        admitted
            .run(move || -> Result<bool> {
                if library.current().fingerprint == observed
                    || fingerprint(&library.paths) != observed
                {
                    return Ok(false);
                }
                let snapshot = Self::build(&library.paths)?;
                let retired = {
                    let mut current = library.inner.write().unwrap();
                    std::mem::replace(&mut *current, Arc::new(snapshot))
                };
                // Closing the last DB/index handle can block. Keep that work
                // on the admitted worker, outside the publication lock.
                drop(retired);
                Ok(true)
            })
            .await?
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
            match self.refresh_observed(fp).await {
                Ok(true) => eprintln!(
                    "otzaria-mcp: library changed → reloaded (data_version={})",
                    self.current().fingerprint
                ),
                Ok(false) => {} // Changed again; retry the debounce next poll.
                Err(e) if matches!(e.downcast_ref(), Some(crate::admission::Error::Closed)) => {
                    return
                }
                Err(e) => eprintln!("otzaria-mcp: reload failed, keeping current snapshot: {e:#}"),
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

/// Fingerprint of file identity, size and timestamps. WAL is included only for
/// live SQLite readers; empty WAL files are equivalent to absent WAL files.
pub fn fingerprint(paths: &Paths) -> String {
    let mut hasher = DefaultHasher::new();
    hash_file(&paths.db, false, &mut hasher);
    if !paths.snapshot {
        let mut wal = paths.db.as_os_str().to_owned();
        wal.push("-wal");
        hash_file(&PathBuf::from(wal), true, &mut hasher);
    }
    if let Some(idx) = &paths.index {
        hash_file(&idx.join("meta.json"), false, &mut hasher);
        hash_file(&idx.join("otzaria_index_meta.json"), false, &mut hasher);
        hash_file(&idx.join("otzaria_book_order.json"), false, &mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

fn hash_file(path: &Path, wal: bool, hasher: &mut impl Hasher) {
    path.hash(hasher);
    let metadata = std::fs::metadata(path).ok();
    let metadata = metadata.filter(|meta| !wal || meta.len() > 0);
    metadata.is_some().hash(hasher);
    if let Some(meta) = metadata {
        meta.len().hash(hasher);
        meta.modified().ok().hash(hasher);
        meta.created().ok().hash(hasher);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            meta.dev().hash(hasher);
            meta.ino().hash(hasher);
            // SQLite may fchown an existing WAL while opening it, changing only
            // ctime. Keep identity/length/mtime; ignore only this permission noise.
            if !wal {
                meta.ctime().hash(hasher);
                meta.ctime_nsec().hash(hasher);
            }
        }
    }
}
