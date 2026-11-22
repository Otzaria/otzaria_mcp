//! The app's secondary book sources: personal books (`user_books.db`) and attached book
//! databases (`.db` files the user linked in "מסדים מצורפים"). Both use the library's own
//! schema (a subset of it), so the same read-only `Db` and find-ref code serve them; only
//! the book ids live in their own number spaces — a hit is identified by its `source` as
//! well as its id, exactly like the app's `BookSource` ('o' / 'u' / 'd:<slug>').
//!
//! Discovery, highest precedence first:
//!   * `--user-books-db <path>` / `OTZARIA_USER_BOOKS_DB`, else
//!     `<app data>/databases/user_books.db`;
//!   * `--attached <slug>=<path>` (repeatable) / `OTZARIA_ATTACHED` (`slug=path` pairs
//!     separated by `;`), else the app's own list: the `key-attached-libraries` value in
//!     `<app data>/app_preferences.hive` (entries with status "ok" that are not hidden,
//!     by priority) — re-read whenever that file changes, so a database attached or
//!     removed in the app shows up without restarting the server;
//!   * `--no-secondary` turns both off.
//!
//! `<app data>` is `--app-data-dir` / `OTZARIA_APP_DATA_DIR`, else the folder that holds
//! the library's `books/` folder when it has an `app_preferences.hive`, else the per-OS
//! data root that has one.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde::Serialize;

use crate::db::Db;

/// Which part of Otzaria a book lives in.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Source {
    /// `seforim.db`, the official library.
    Library,
    /// `user_books.db`, the user's personal books.
    User,
    /// An attached database, by its slug.
    Attached(String),
}

impl Source {
    /// The wire name used in tool results: "library", "user" or "attached".
    pub fn kind(&self) -> &'static str {
        match self {
            Source::Library => "library",
            Source::User => "user",
            Source::Attached(_) => "attached",
        }
    }

    pub fn slug(&self) -> Option<&str> {
        match self {
            Source::Attached(s) => Some(s),
            _ => None,
        }
    }

    /// The app's catalogue key for a book of this source: `id:<n>` (library),
    /// `uid:<n>` (personal), `db:<slug>:<n>` (attached) — the plugin SDK's `bookUid`.
    pub fn book_uid(&self, book_id: i64) -> String {
        match self {
            Source::Library => format!("id:{book_id}"),
            Source::User => format!("uid:{book_id}"),
            Source::Attached(slug) => format!("db:{slug}:{book_id}"),
        }
    }

    /// The `source` query parameter of an `otzaria://open/book/<id>` deep link
    /// (absent for the library).
    pub fn link_param(&self) -> Option<String> {
        match self {
            Source::Library => None,
            Source::User => Some("user".to_string()),
            Source::Attached(slug) => Some(format!("db:{slug}")),
        }
    }

    /// Default order between sources, as in the app: library, personal, attached.
    pub fn rank(&self) -> u8 {
        match self {
            Source::Library => 0,
            Source::User => 1,
            Source::Attached(_) => 2,
        }
    }

    /// Parse a `sources` filter entry: "library" / "user" / "attached" / "attached:<slug>".
    pub fn matches_filter(&self, filter: &str) -> bool {
        let f = filter.trim();
        match self {
            Source::Library => f == "library" || f == "official",
            Source::User => f == "user" || f == "personal",
            Source::Attached(slug) => {
                f == "attached" || f.strip_prefix("attached:").is_some_and(|s| s == slug)
            }
        }
    }
}

impl Serialize for Source {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.kind())
    }
}

/// One secondary database to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecondarySpec {
    pub source: Source,
    pub path: PathBuf,
    /// Display name (the attached library's `displayName`; "ספרים אישיים" for personal books).
    pub name: String,
    /// Open with `immutable=1` (the app does so for WAL databases in read-only folders).
    pub immutable: bool,
}

/// How to find the secondary databases (from flags / env; see the module docs).
#[derive(Debug, Clone, Default)]
pub struct SecondaryConfig {
    pub disabled: bool,
    pub app_data_dir: Option<PathBuf>,
    pub user_books_db: Option<PathBuf>,
    /// Explicit `slug=path` pairs; when non-empty the app's list is not read.
    pub attached: Vec<(String, PathBuf)>,
}

impl SecondaryConfig {
    /// The app-preferences file the attached list is read from, when it is.
    pub fn prefs_file(&self) -> Option<PathBuf> {
        if self.disabled || !self.attached.is_empty() {
            return None;
        }
        self.app_data_dir
            .as_ref()
            .map(|d| d.join("app_preferences.hive"))
            .filter(|p| p.is_file())
    }

    /// The databases to serve right now.
    pub fn discover(&self) -> Vec<SecondarySpec> {
        let mut out = Vec::new();
        if self.disabled {
            return out;
        }
        let user = self.user_books_db.clone().or_else(|| {
            self.app_data_dir
                .as_ref()
                .map(|d| d.join("databases").join("user_books.db"))
        });
        if let Some(p) = user.filter(|p| p.is_file()) {
            out.push(SecondarySpec {
                source: Source::User,
                path: p,
                name: "ספרים אישיים".to_string(),
                immutable: false,
            });
        }
        if !self.attached.is_empty() {
            for (slug, path) in &self.attached {
                out.push(SecondarySpec {
                    source: Source::Attached(slug.clone()),
                    path: path.clone(),
                    name: slug.clone(),
                    immutable: false,
                });
            }
        } else if let Some(prefs) = self.prefs_file() {
            if let Ok(bytes) = std::fs::read(&prefs) {
                out.extend(attached_from_hive(&bytes));
            }
        }
        out
    }

    /// Changes whenever the set of databases or any of their files changes.
    pub fn fingerprint(&self) -> String {
        let mut files: Vec<PathBuf> = Vec::new();
        for spec in self.discover() {
            let wal = PathBuf::from(format!("{}-wal", spec.path.display()));
            files.push(spec.path);
            files.push(wal);
        }
        let mut fp = crate::reload::hash_paths(&files);
        // the list itself (a database attached/removed without touching the others)
        if let Some(prefs) = self.prefs_file() {
            if let Ok(bytes) = std::fs::read(&prefs) {
                let list: Vec<String> = attached_from_hive(&bytes)
                    .into_iter()
                    .map(|s| format!("{}={}", s.source.slug().unwrap_or(""), s.path.display()))
                    .collect();
                fp.push(':');
                fp.push_str(&crate::reload::hash_strings(&list));
            }
        }
        fp
    }
}

/// The attached libraries the app lists, from the raw bytes of `app_preferences.hive`.
///
/// Hive appends a frame per write — `[u32 len][key type 1][u8 key len][key][value][crc]`,
/// a string value being `[0x04][u32 len][utf-8]` — so the LAST frame for the key holds
/// the current value (a deleting frame carries no value). The value is the JSON list the
/// app's `AttachedLibraryStore` saves.
pub fn attached_from_hive(bytes: &[u8]) -> Vec<SecondarySpec> {
    const KEY: &[u8] = b"key-attached-libraries";
    let mut needle = vec![KEY.len() as u8];
    needle.extend_from_slice(KEY);
    let mut last: Option<Option<&[u8]>> = None;
    let mut from = 0;
    while let Some(pos) = find(&bytes[from..], &needle).map(|p| p + from) {
        let v = pos + needle.len();
        from = pos + 1;
        // a string key (type byte 0x01 before its length), not text inside a value
        if pos == 0 || bytes[pos - 1] != 0x01 {
            continue;
        }
        if bytes.get(v) == Some(&0x04) && v + 5 <= bytes.len() {
            let len = u32::from_le_bytes([bytes[v + 1], bytes[v + 2], bytes[v + 3], bytes[v + 4]])
                as usize;
            let start = v + 5;
            if start + len <= bytes.len() {
                last = Some(Some(&bytes[start..start + len]));
                continue;
            }
        }
        // a frame with no string value: the key was deleted
        last = Some(None);
    }
    let Some(Some(json)) = last else {
        return vec![];
    };
    parse_attached_json(json)
}

/// The app's attached-libraries JSON list → the databases to open, by priority.
pub fn parse_attached_json(json: &[u8]) -> Vec<SecondarySpec> {
    let Ok(serde_json::Value::Array(items)) = serde_json::from_slice::<serde_json::Value>(json)
    else {
        return vec![];
    };
    let mut out: Vec<(i64, usize, SecondarySpec)> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let get = |k: &str| item.get(k).and_then(|v| v.as_str());
        let (Some(slug), Some(path)) = (get("slug"), get("path")) else {
            continue;
        };
        let ok = get("status").is_none_or(|s| s == "ok");
        let hidden = item
            .get("hidden")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !ok || hidden || !valid_slug(slug) {
            continue;
        }
        let priority = item.get("priority").and_then(|v| v.as_i64()).unwrap_or(0);
        out.push((
            priority,
            i,
            SecondarySpec {
                source: Source::Attached(slug.to_string()),
                path: PathBuf::from(path),
                name: get("displayName").unwrap_or(slug).to_string(),
                immutable: item
                    .get("immutable")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            },
        ));
    }
    out.sort_by_key(|(p, i, _)| (*p, *i));
    out.into_iter().map(|(_, _, s)| s).collect()
}

/// The app's `BookSource.isValidSlug`: letters, digits and `._-`, 1–64 chars.
fn valid_slug(slug: &str) -> bool {
    let n = slug.chars().count();
    (1..=64).contains(&n)
        && slug
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The app data folder: explicit, else next to the library's `books/` folder, else a
/// per-OS root — whichever has the app's preferences or databases.
pub fn app_data_dir(explicit: Option<PathBuf>, db: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    if explicit.is_some() {
        return explicit;
    }
    let looks_like = |d: &Path| {
        d.join("app_preferences.hive").is_file()
            || d.join("databases").join("user_books.db").is_file()
    };
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(gp) = db.parent().and_then(Path::parent) {
        candidates.push(gp.to_path_buf());
    }
    candidates.extend(roots.iter().cloned());
    candidates.into_iter().find(|d| looks_like(d))
}

/// A secondary database, opened on first use. A database that fails to open is
/// reported once and skipped; the rest keep working.
pub struct SecondaryDb {
    pub spec: SecondarySpec,
    db: OnceLock<Option<Arc<Db>>>,
}

impl SecondaryDb {
    pub fn new(spec: SecondarySpec) -> SecondaryDb {
        SecondaryDb {
            spec,
            db: OnceLock::new(),
        }
    }

    pub fn db(&self) -> Option<Arc<Db>> {
        self.db
            .get_or_init(
                || match Db::open_secondary(&self.spec.path, self.spec.immutable) {
                    Ok(db) => {
                        eprintln!(
                            "otzaria-mcp: opened {} source {} ({})",
                            self.spec.source.kind(),
                            self.spec.name,
                            self.spec.path.display()
                        );
                        Some(Arc::new(db))
                    }
                    Err(e) => {
                        eprintln!(
                            "otzaria-mcp: WARNING skipping {} source {}: {e:#}",
                            self.spec.source.kind(),
                            self.spec.path.display()
                        );
                        None
                    }
                },
            )
            .clone()
    }
}

/// The secondary databases of one moment, with the fingerprint they were built from.
pub struct SecondarySet {
    pub fingerprint: String,
    pub dbs: Vec<Arc<SecondaryDb>>,
}

impl SecondarySet {
    pub fn build(config: &SecondaryConfig) -> SecondarySet {
        let fingerprint = config.fingerprint();
        let dbs = config
            .discover()
            .into_iter()
            .map(|s| Arc::new(SecondaryDb::new(s)))
            .collect();
        SecondarySet { fingerprint, dbs }
    }

    pub fn empty() -> SecondarySet {
        SecondarySet {
            fingerprint: String::new(),
            dbs: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(key: &str, value: Option<&str>) -> Vec<u8> {
        let mut body = vec![0x01, key.len() as u8];
        body.extend_from_slice(key.as_bytes());
        if let Some(v) = value {
            body.push(0x04);
            body.extend_from_slice(&(v.len() as u32).to_le_bytes());
            body.extend_from_slice(v.as_bytes());
        }
        let mut out = ((body.len() + 8) as u32).to_le_bytes().to_vec();
        out.extend(body);
        out.extend_from_slice(&[0, 0, 0, 0]);
        out
    }

    #[test]
    fn hive_last_frame_wins_and_filters() {
        let old = r#"[{"slug":"a","path":"/x/a.db","status":"ok"}]"#;
        let new = r#"[{"slug":"b","displayName":"בי","path":"/x/b.db","status":"ok","priority":1},
                      {"slug":"c","path":"/x/c.db","status":"ok","priority":0},
                      {"slug":"d","path":"/x/d.db","status":"missing"},
                      {"slug":"e","path":"/x/e.db","status":"ok","hidden":true},
                      {"slug":"bad slug","path":"/x/f.db"}]"#;
        let mut bytes = frame("key-attached-library-folders", Some("[]"));
        bytes.extend(frame("key-attached-libraries", Some(old)));
        bytes.extend(frame("key-dark-mode", Some("x")));
        bytes.extend(frame("key-attached-libraries", Some(new)));
        let got = attached_from_hive(&bytes);
        let slugs: Vec<_> = got.iter().map(|s| s.source.slug().unwrap()).collect();
        assert_eq!(slugs, vec!["c", "b"]);
        assert_eq!(got[1].name, "בי");
        assert_eq!(got[0].path, PathBuf::from("/x/c.db"));
    }

    #[test]
    fn hive_deleted_key_means_none() {
        let mut bytes = frame(
            "key-attached-libraries",
            Some(r#"[{"slug":"a","path":"/a"}]"#),
        );
        bytes.extend(frame("key-attached-libraries", None));
        assert!(attached_from_hive(&bytes).is_empty());
    }

    #[test]
    fn book_uid_and_link_param() {
        let s = Source::Attached("otzarya-unofficial-books".into());
        assert_eq!(s.book_uid(7), "db:otzarya-unofficial-books:7");
        assert_eq!(
            s.link_param().as_deref(),
            Some("db:otzarya-unofficial-books")
        );
        assert_eq!(Source::User.book_uid(3), "uid:3");
        assert_eq!(Source::Library.link_param(), None);
        assert!(s.matches_filter("attached"));
        assert!(s.matches_filter("attached:otzarya-unofficial-books"));
        assert!(!s.matches_filter("attached:other"));
    }
}

#[cfg(test)]
mod resolve_tests {
    use crate::db::Db;
    use crate::refs::ResolveOptions;
    use rusqlite::Connection;

    /// An attached database's shape: no heRef or acronyms on `book`, a TOC per book.
    fn attached_fixture() -> Db {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE book (id INTEGER PRIMARY KEY, categoryId INTEGER, title TEXT,
                 orderIndex INTEGER, totalLines INTEGER, isBaseBook INTEGER, fileType TEXT);
             CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER,
                 heRef TEXT, content TEXT, tocEntryId INTEGER);
             CREATE TABLE tocText (id INTEGER PRIMARY KEY, text TEXT);
             CREATE TABLE tocEntry (id INTEGER PRIMARY KEY, bookId INTEGER, parentId INTEGER,
                 textId INTEGER, level INTEGER, lineId INTEGER, isLastChild INTEGER,
                 hasChildren INTEGER);
             INSERT INTO book VALUES (7, 1, 'חדושי הר״ן על תלמוד בבלי (מהדורה שונה)', 1, 9, 0, 'txt');
             INSERT INTO book VALUES (8, 1, 'קונטרס הספיקות', 2, 1, 0, 'txt');
             INSERT INTO line VALUES (1, 7, 0, NULL, 'חדושי הר״ן', NULL);
             INSERT INTO line VALUES (2, 7, 1, NULL, 'מסכת גיטין', NULL);
             INSERT INTO line VALUES (3, 7, 2, NULL, 'דף עה:', NULL);
             INSERT INTO line VALUES (4, 7, 3, NULL, 'גמרא. ...', NULL);
             INSERT INTO line VALUES (5, 8, 0, NULL, 'קונטרס הספיקות', NULL);
             INSERT INTO tocText VALUES (1, 'גיטין'), (2, 'דף עה:');
             INSERT INTO tocEntry VALUES (1, 7, NULL, 1, 1, 2, 0, 1);
             INSERT INTO tocEntry VALUES (2, 7, 1, 2, 2, 3, 1, 0);",
        )
        .unwrap();
        Db::from_connection(conn).unwrap()
    }

    const OPTS: ResolveOptions = ResolveOptions {
        heref_fallback: false,
        app_detection: true,
    };

    #[test]
    fn a_book_named_inside_its_title_resolves_by_toc() {
        let db = attached_fixture();
        let c = db.resolve_ref_with("הר\"ן (גיטין עה,ב)", 5, OPTS).unwrap();
        assert_eq!((c[0].book_id, c[0].line_index), (7, 2), "{c:?}");
        assert!(c[0].reference.contains("דף עה"), "{}", c[0].reference);
    }

    #[test]
    fn a_title_alone_is_the_book() {
        let db = attached_fixture();
        let c = db.resolve_ref_with("בקונטרס הספיקות", 5, OPTS).unwrap();
        assert_eq!((c[0].book_id, c[0].match_kind), (8, "book"), "{c:?}");
        assert_eq!(c[0].rank, 0);
    }

    #[test]
    fn search_books_reads_a_secondary_schema() {
        let db = attached_fixture();
        let hits = db.search_books("הספיקות", None, 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].book_id, hits[0].match_rank), (8, 3)); // title contains
        let mut h = db.search_books("הר״ן", None, 5).unwrap().remove(0);
        h.set_source(&super::Source::Attached("x".into()));
        assert_eq!(h.link, "otzaria://open/book/7?source=db:x&index=0");
        assert_eq!(h.book_uid, "db:x:7");
    }
}
