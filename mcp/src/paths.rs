//! Runtime path discovery for Otzaria's data, mirroring `lib/core/app_paths.dart`
//! but with an explicit precedence and NO Hive reading (Hive is a fragile,
//! unnecessary dependency). Precedence, highest first:
//!   1. explicit flags:  --db / --index / --lexical-db / --dictionaries-dir
//!   2. env vars:        OTZARIA_DB / OTZARIA_INDEX / OTZARIA_LEXICAL_DB / OTZARIA_DICTIONARIES_DIR
//!   3. --data-dir / OTZARIA_DATA_DIR  (two supported layouts, see below)
//!   4. per-OS defaults (see below).
//!
//! Per-OS data roots, per-user install first, then system-wide install:
//! - Windows: `%APPDATA%\otzaria`, then `%ProgramData%\otzaria`
//! - macOS: `~/Library/Application Support/otzaria`, then `/Library/Application Support/otzaria`
//! - Linux: `$XDG_DATA_HOME/otzaria` (`~/.local/share/otzaria`), then `/var/lib/otzaria`
//!
//! Inside a root the library is `books/seforim.db` and the index `index/` (db and index
//! are resolved INDEPENDENTLY — the app keeps the index next to the library folder, or
//! under the data root for older installs).
//!
//! `--data-dir <root>` supports two layouts, auto-detected by what exists:
//!   * download root:      `<root>/otzaria/seforim.db` + `<root>/index`
//!   * library dir itself: `<root>/seforim.db` + `<root>/index` (adjacent)
//!
//! `lexical.db` sits next to `seforim.db` (as in the app). The advanced-search
//! dictionaries (`dictionary.json`, `Acronyms.json`) are materialised by the app under
//! `<data root>/dictionaries`.

use anyhow::{bail, Result};

use crate::secondary::SecondaryConfig;
use std::path::PathBuf;

/// Overrides collected from CLI flags and env vars, before resolution.
#[derive(Debug, Default, Clone)]
pub struct PathArgs {
    pub db: Option<PathBuf>,
    pub index: Option<PathBuf>,
    pub lexical_db: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub dictionaries_dir: Option<PathBuf>,
    /// `--snapshot`: open the DB with `immutable=1` (closed snapshot, ignores WAL).
    /// Off by default so live WAL writes by the app are read consistently.
    pub snapshot: bool,
    /// The app's data folder (`app_preferences.hive`, `databases/user_books.db`).
    pub app_data_dir: Option<PathBuf>,
    /// Personal books database (`user_books.db`).
    pub user_books_db: Option<PathBuf>,
    /// Attached databases as `slug=path` (replaces the app's own list).
    pub attached: Vec<(String, PathBuf)>,
    /// `--no-secondary`: serve the library only.
    pub no_secondary: bool,
}

/// Resolved, existence-checked paths the server runs against.
#[derive(Debug, Clone)]
pub struct Paths {
    pub db: PathBuf,
    pub index: Option<PathBuf>,
    pub lexical_db: Option<PathBuf>,
    /// Folder holding `dictionary.json` / `Acronyms.json` (advanced-search lexicons).
    pub dictionaries_dir: Option<PathBuf>,
    pub snapshot: bool,
    /// Where personal books and attached databases are found (see `secondary`).
    pub secondary: SecondaryConfig,
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
        self.dictionaries_dir = self
            .dictionaries_dir
            .or_else(|| env_path("OTZARIA_DICTIONARIES_DIR"));
        self.app_data_dir = self
            .app_data_dir
            .or_else(|| env_path("OTZARIA_APP_DATA_DIR"));
        self.user_books_db = self
            .user_books_db
            .or_else(|| env_path("OTZARIA_USER_BOOKS_DB"));
        if self.attached.is_empty() {
            if let Ok(v) = std::env::var("OTZARIA_ATTACHED") {
                for pair in v.split(';').filter(|p| !p.trim().is_empty()) {
                    if let Some(a) = parse_attached_arg(pair) {
                        self.attached.push(a);
                    }
                }
            }
        }
        self
    }

    pub fn resolve(self) -> Result<Paths> {
        let dd = self.data_dir.as_deref();
        let roots = os_data_roots();

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
            for r in &roots {
                candidates.push(r.join("books").join("seforim.db"));
                candidates.push(r.join("books").join("אוצריא").join("seforim.db"));
                candidates.push(r.join("seforim.db"));
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
            for r in &roots {
                candidates.push(r.join("index"));
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

        // --- advanced-search dictionaries (optional) ---
        let dictionaries_dir = self.dictionaries_dir.or_else(|| {
            let mut candidates: Vec<PathBuf> = Vec::new();
            if let Some(d) = dd {
                candidates.push(d.join("dictionaries"));
            }
            if let Some(parent) = db.parent() {
                candidates.push(parent.join("dictionaries"));
                if let Some(gp) = parent.parent() {
                    candidates.push(gp.join("dictionaries"));
                }
            }
            for r in &roots {
                candidates.push(r.join("dictionaries"));
            }
            candidates
                .into_iter()
                .find(|p| p.join("dictionary.json").is_file() || p.join("Acronyms.json").is_file())
        });

        let app_data_dir = if self.no_secondary {
            None
        } else {
            crate::secondary::app_data_dir(self.app_data_dir, &db, &roots)
        };
        let secondary = SecondaryConfig {
            disabled: self.no_secondary,
            app_data_dir,
            user_books_db: self.user_books_db,
            attached: self.attached,
        };

        Ok(Paths {
            db,
            index,
            lexical_db,
            dictionaries_dir,
            snapshot: self.snapshot,
            secondary,
        })
    }
}

/// `slug=path` → (slug, path).
pub fn parse_attached_arg(arg: &str) -> Option<(String, PathBuf)> {
    let (slug, path) = arg.split_once('=')?;
    let (slug, path) = (slug.trim(), path.trim());
    (!slug.is_empty() && !path.is_empty()).then(|| (slug.to_string(), PathBuf::from(path)))
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

/// Per-OS Otzaria data roots: the per-user root first, then the system-wide one.
fn os_data_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    #[cfg(target_os = "macos")]
    {
        if let Some(h) = std::env::var_os("HOME") {
            roots.push(
                PathBuf::from(h)
                    .join("Library")
                    .join("Application Support")
                    .join("otzaria"),
            );
        }
        roots.push(PathBuf::from("/Library/Application Support/otzaria"));
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(a) = std::env::var_os("APPDATA") {
            roots.push(PathBuf::from(a).join("otzaria"));
        }
        let pd = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        roots.push(pd.join("otzaria"));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(d) = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share"))
            })
        {
            roots.push(d.join("otzaria"));
        }
        roots.push(PathBuf::from("/var/lib/otzaria"));
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::PathArgs;

    #[test]
    fn data_dir_download_layout_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("otzaria")).unwrap();
        std::fs::write(root.join("otzaria").join("seforim.db"), b"").unwrap();
        std::fs::write(root.join("otzaria").join("lexical.db"), b"").unwrap();
        std::fs::create_dir_all(root.join("index")).unwrap();
        std::fs::write(root.join("index").join("meta.json"), b"{}").unwrap();
        std::fs::create_dir_all(root.join("dictionaries")).unwrap();
        std::fs::write(root.join("dictionaries").join("Acronyms.json"), b"{}").unwrap();

        let paths = PathArgs {
            data_dir: Some(root.to_path_buf()),
            ..PathArgs::default()
        }
        .resolve()
        .unwrap();
        assert_eq!(paths.db, root.join("otzaria").join("seforim.db"));
        assert_eq!(paths.index.as_deref(), Some(root.join("index").as_path()));
        assert_eq!(
            paths.lexical_db.as_deref(),
            Some(root.join("otzaria").join("lexical.db").as_path())
        );
        assert_eq!(
            paths.dictionaries_dir.as_deref(),
            Some(root.join("dictionaries").as_path())
        );
    }

    #[test]
    fn explicit_index_must_be_a_tantivy_index() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("seforim.db");
        std::fs::write(&db, b"").unwrap();
        let err = PathArgs {
            db: Some(db),
            index: Some(dir.path().join("nope")),
            ..PathArgs::default()
        }
        .resolve();
        assert!(err.is_err());
    }
}
