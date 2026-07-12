//! Runtime path discovery for Otzaria's data, mirroring `lib/core/app_paths.dart`
//! but with an explicit precedence and NO Hive reading (Hive is a fragile,
//! unnecessary dependency). Precedence, highest first:
//!   1. explicit flags:  --db / --index / --lexical-db
//!   2. env vars:        OTZARIA_DB / OTZARIA_INDEX / OTZARIA_LEXICAL_DB
//!   3. --data-dir / OTZARIA_DATA_DIR  (two supported layouts, see below)
//!   4. per-OS defaults  (db and index resolved INDEPENDENTLY — in an installed
//!      app the DB may live under `<support>/books/` while the index is under
//!      `<support>/index/`).
//!
//! `--data-dir <root>` supports two layouts, auto-detected by what exists:
//!   * download root:      `<root>/otzaria/seforim.db` + `<root>/index`
//!   * library dir itself: `<root>/seforim.db` + `<root>/index` (adjacent)

use anyhow::{bail, Result};
use std::path::PathBuf;

/// Overrides collected from CLI flags and env vars, before resolution.
#[derive(Debug, Default, Clone)]
pub struct PathArgs {
    pub db: Option<PathBuf>,
    pub index: Option<PathBuf>,
    pub lexical_db: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    /// `--snapshot`: open the DB with `immutable=1` (closed snapshot, ignores WAL).
    /// Off by default so live WAL writes by the app are read consistently.
    pub snapshot: bool,
}

/// Resolved, existence-checked paths the server runs against.
#[derive(Debug, Clone)]
pub struct Paths {
    pub db: PathBuf,
    pub index: Option<PathBuf>,
    pub lexical_db: Option<PathBuf>,
    pub snapshot: bool,
}

impl PathArgs {
    /// Fold env vars in, without overriding values already set from flags.
    pub fn with_env(mut self) -> Self {
        fn env_path(key: &str) -> Option<PathBuf> {
            std::env::var_os(key).map(PathBuf::from)
        }
        self.db = self.db.or_else(|| env_path("OTZARIA_DB"));
        self.index = self.index.or_else(|| env_path("OTZARIA_INDEX"));
        self.lexical_db = self.lexical_db.or_else(|| env_path("OTZARIA_LEXICAL_DB"));
        self.data_dir = self.data_dir.or_else(|| env_path("OTZARIA_DATA_DIR"));
        self
    }

    pub fn resolve(self) -> Result<Paths> {
        let dd = self.data_dir.as_deref();
        let support = os_support_dir();

        // --- seforim.db (required) ---
        let db = if let Some(p) = self.db {
            if !p.is_file() {
                bail!(
                    "seforim.db not found at --db/OTZARIA_DB path: {}",
                    p.display()
                );
            }
            p
        } else {
            let mut candidates: Vec<PathBuf> = Vec::new();
            if let Some(d) = dd {
                candidates.push(d.join("otzaria").join("seforim.db")); // download-root layout
                candidates.push(d.join("seforim.db")); // library-dir layout
            }
            if let Some(s) = &support {
                candidates.push(s.join("books").join("seforim.db"));
                candidates.push(s.join("seforim.db"));
            }
            first_file(&candidates).ok_or_else(|| {
                anyhow::anyhow!(
                    "could not locate seforim.db. Pass --db <path> or --data-dir <otzaria root>. \
                     Tried: {}",
                    display_list(&candidates)
                )
            })?
        };

        // --- Tantivy index dir (optional; a Tantivy index has a meta.json) ---
        // An explicit --index that isn't a real index is a hard error; an
        // auto-detected miss is simply "no index" (index-backed tools disabled).
        let index = if let Some(p) = self.index {
            if !p.join("meta.json").is_file() {
                bail!(
                    "--index/OTZARIA_INDEX is not a Tantivy index (no meta.json): {}",
                    p.display()
                );
            }
            Some(p)
        } else {
            let mut candidates: Vec<PathBuf> = Vec::new();
            if let Some(d) = dd {
                candidates.push(d.join("index"));
                candidates.push(d.join("otzaria").join("index"));
            }
            if let Some(parent) = db.parent() {
                candidates.push(parent.join("index"));
                if let Some(gp) = parent.parent() {
                    candidates.push(gp.join("index"));
                }
            }
            if let Some(s) = &support {
                candidates.push(s.join("index"));
            }
            first_index_dir(&candidates)
        };

        // --- lexical.db (optional, only affects fuzzy morphology) ---
        let lexical_db = self.lexical_db.or_else(|| {
            let mut candidates: Vec<PathBuf> = Vec::new();
            if let Some(d) = dd {
                candidates.push(d.join("otzaria").join("lexical.db"));
                candidates.push(d.join("lexical.db"));
            }
            if let Some(parent) = db.parent() {
                candidates.push(parent.join("lexical.db"));
            }
            first_file(&candidates)
        });

        Ok(Paths {
            db,
            index,
            lexical_db,
            snapshot: self.snapshot,
        })
    }
}

fn first_file(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|p| p.is_file()).cloned()
}

/// A directory that looks like a Tantivy index (has a `meta.json`).
fn first_index_dir(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates
        .iter()
        .find(|p| p.is_dir() && p.join("meta.json").is_file())
        .cloned()
}

fn display_list(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Per-OS application-support root for otzaria.
fn os_support_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|h| {
            PathBuf::from(h)
                .join("Library")
                .join("Application Support")
                .join("otzaria")
        })
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("otzaria"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share"))
            })
            .map(|d| d.join("otzaria"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", unix)))]
    {
        None
    }
}
