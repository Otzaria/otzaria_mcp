//! Read-only access to Otzaria's `seforim.db`.
//!
//! Supports the current library (schema 6 / db_version 32: text split into
//! `line_content` and compressed with zstd dictionaries, `version_line`, `line_ref`,
//! `line_dh`, `link_anchor`, `link_coverage`, `link_suppressed_side`, publication data,
//! category descriptions, …) and degrades gracefully on older databases (schema ≤ 5,
//! plain `line.content`): every optional table/column is probed once at open time
//! ([`Caps`]). The DB is opened with `SQLITE_OPEN_READ_ONLY` (WAL-aware) by default, or
//! `immutable=1` only in explicit `--snapshot` mode, and `PRAGMA query_only=ON`.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard, OnceLock};

use anyhow::{Context, Result};
use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Serialize;

use crate::paths::Paths;
use crate::refs::{self, RefCandidate, RefIndex};
use crate::text::{self, LineCodec};

pub use crate::text::strip_html_tags;

/// A decoded line: (lineIndex, line.id, heRef, html).
pub type LineRow = (i64, i64, Option<String>, String);

/// The connection plus the codec for its compressed rows (one per connection).
pub struct Inner {
    pub conn: Connection,
    pub codec: LineCodec,
}

/// What the opened database has. Probed once; every query adapts to it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Caps {
    /// `line_content` table (schema 6): text is not in `line.content`.
    pub split_content: bool,
    /// Rows are zstd frames (a `zstd_dict` table exists).
    pub compressed: bool,
    pub line_ref: bool,
    pub line_dh: bool,
    pub link_anchor: bool,
    pub link_range: bool,
    pub link_coverage: bool,
    pub link_suppressed_side: bool,
    pub link_base_provenance: bool,
    pub book_versions: bool,
    pub version_line: bool,
    pub pub_dates: bool,
    pub pub_places: bool,
    pub book_base_text: bool,
    pub default_targum: bool,
    pub book_has_links: bool,
    pub category_closure: bool,
    pub category_descriptions: bool,
    pub book_dependence: bool,
    pub book_generation: bool,
    pub alt_toc: bool,
}

impl Caps {
    fn probe(conn: &Connection) -> Result<Caps> {
        let mut tables = HashSet::new();
        let mut stmt = conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?;
        for name in stmt.query_map([], |r| r.get::<_, String>(0))? {
            tables.insert(name?);
        }
        let columns = |table: &str| -> Result<HashSet<String>> {
            let mut set = HashSet::new();
            if !tables.contains(table) {
                return Ok(set);
            }
            let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
            for name in stmt.query_map([], |r| r.get::<_, String>(1))? {
                set.insert(name?);
            }
            Ok(set)
        };
        let t = |name: &str| tables.contains(name);
        let book_cols = columns("book")?;
        let link_cols = columns("link")?;
        let cat_cols = columns("category")?;
        Ok(Caps {
            split_content: t("line_content"),
            compressed: t("zstd_dict"),
            line_ref: t("line_ref"),
            line_dh: t("line_dh"),
            link_anchor: t("link_anchor"),
            link_range: t("link_range"),
            link_coverage: t("link_coverage"),
            link_suppressed_side: t("link_suppressed_side"),
            link_base_provenance: link_cols.contains("baseProvenance"),
            book_versions: t("book_version"),
            version_line: t("version_line"),
            pub_dates: t("book_pub_date") && t("pub_date"),
            pub_places: t("book_pub_place") && t("pub_place"),
            book_base_text: t("book_base_text"),
            default_targum: t("default_targum"),
            book_has_links: t("book_has_links"),
            category_closure: t("category_closure"),
            category_descriptions: cat_cols.contains("heDesc") && cat_cols.contains("heShortDesc"),
            book_dependence: book_cols.contains("dependenceType")
                && book_cols.contains("collectiveTitleHe"),
            book_generation: t("book_generation") && t("generation"),
            alt_toc: t("alt_toc_structure") && t("alt_toc_entry"),
        })
    }
}

/// Historical era of a book (the app's `CommentaryEra`): the book's earliest
/// generation in `book_generation`, mapped by name. Books with no generation are `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Era {
    TorahShebichtav = 0,
    Chazal = 1,
    Rishonim = 2,
    Acharonim = 3,
    Modern = 4,
    Other = 5,
}

impl Era {
    pub const ALL: [Era; 6] = [
        Era::TorahShebichtav,
        Era::Chazal,
        Era::Rishonim,
        Era::Acharonim,
        Era::Modern,
        Era::Other,
    ];

    pub fn hebrew_name(self) -> &'static str {
        match self {
            Era::TorahShebichtav => "תורה שבכתב",
            Era::Chazal => "חז\"ל",
            Era::Rishonim => "ראשונים",
            Era::Acharonim => "אחרונים",
            Era::Modern => "מחברי זמננו",
            Era::Other => "שאר מפרשים",
        }
    }

    pub fn order(self) -> u8 {
        self as u8
    }

    /// Map a `generation.name` to an era, ignoring gershayim (like `GenerationCache`).
    pub fn from_generation_name(name: &str) -> Era {
        let strip = |s: &str| -> String {
            s.chars()
                .filter(|c| !matches!(c, '"' | '\'' | '\u{05F4}' | '\u{05F3}'))
                .collect::<String>()
                .trim()
                .to_string()
        };
        let n = strip(name);
        Era::ALL
            .into_iter()
            .find(|e| strip(e.hebrew_name()) == n)
            .unwrap_or(Era::Other)
    }

    /// Parse a user-facing era: the Hebrew name or an English key.
    pub fn parse(s: &str) -> Option<Era> {
        let lower = s.trim().to_lowercase();
        let by_key = match lower.as_str() {
            "torah" | "torah_shebichtav" | "tanakh" => Some(Era::TorahShebichtav),
            "chazal" => Some(Era::Chazal),
            "rishonim" => Some(Era::Rishonim),
            "acharonim" => Some(Era::Acharonim),
            "modern" => Some(Era::Modern),
            "other" => Some(Era::Other),
            _ => None,
        };
        by_key.or_else(|| {
            let e = Era::from_generation_name(s);
            (e != Era::Other || s.contains("שאר")).then_some(e)
        })
    }
}

impl Serialize for Era {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.hebrew_name())
    }
}

pub struct Db {
    inner: Mutex<Inner>,
    pub caps: Caps,
    /// Lazily-built normalized book-name index for resolve_ref (find_ref cache).
    ref_index: OnceLock<RefIndex>,
    /// bookId → era (earliest generation), built lazily from `book_generation`.
    eras: OnceLock<HashMap<i64, Era>>,
}

impl Db {
    pub fn open(paths: &Paths) -> Result<Db> {
        let conn = if paths.snapshot {
            let uri = format!("file:{}?immutable=1", paths.db.display());
            Connection::open_with_flags(
                uri,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
            )
        } else {
            Connection::open_with_flags(&paths.db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        }
        .with_context(|| format!("opening seforim.db at {}", paths.db.display()))?;
        Db::from_connection(conn)
    }

    /// Wrap an already-open connection (also used by tests with in-memory fixtures).
    pub fn from_connection(conn: Connection) -> Result<Db> {
        conn.execute_batch("PRAGMA query_only=ON;")
            .context("setting query_only pragma")?;
        let caps = Caps::probe(&conn).context("probing seforim.db tables")?;
        let codec = LineCodec::load(&conn).context("loading zstd dictionaries")?;
        Ok(Db {
            inner: Mutex::new(Inner { conn, codec }),
            caps,
            ref_index: OnceLock::new(),
            eras: OnceLock::new(),
        })
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ── content SQL fragments ────────────────────────────────────────────────
    /// The content column for a `line l` row.
    pub(crate) fn content_col(&self) -> &'static str {
        if self.caps.split_content {
            "lc.content"
        } else {
            "l.content"
        }
    }

    /// The join that makes [`Self::content_col`] available.
    pub(crate) fn content_join(&self) -> &'static str {
        if self.caps.split_content {
            "LEFT JOIN line_content lc ON lc.id = l.id"
        } else {
            ""
        }
    }

    /// Decoded text of the given `line.id`s (batched), as raw HTML.
    pub(crate) fn texts_by_line_id(
        &self,
        inner: &mut Inner,
        ids: &[i64],
    ) -> Result<HashMap<i64, String>> {
        let mut out = HashMap::new();
        let mut unique: Vec<i64> = ids.to_vec();
        unique.sort_unstable();
        unique.dedup();
        for chunk in unique.chunks(400) {
            let ph = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT l.id, {} FROM line l {} WHERE l.id IN ({ph})",
                self.content_col(),
                self.content_join()
            );
            let rows: Vec<(i64, Value)> = {
                let mut stmt = inner.conn.prepare(&sql)?;
                let rows = stmt
                    .query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            };
            for (id, v) in rows {
                out.insert(id, inner.codec.text_lossy(&v));
            }
        }
        Ok(out)
    }

    /// Stored endpoints for the 0.1 JSON fields, fetched and decoded in batches.
    pub(crate) fn legacy_link_fields(
        &self,
        link_ids: &[i64],
    ) -> Result<HashMap<i64, serde_json::Value>> {
        struct Endpoint {
            book_id: i64,
            title: String,
            line_id: i64,
            line_index: i64,
            he_ref: Option<String>,
        }

        let mut inner = self.lock();
        let mut ids = link_ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let mut endpoints = Vec::new();
        for chunk in ids.chunks(400) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let mut stmt = inner.conn.prepare(&format!(
                "SELECT l.id, sb.id, sb.title, sl.id, sl.lineIndex, sl.heRef,
                        tb.id, tb.title, tl.id, tl.lineIndex, tl.heRef
                   FROM link l JOIN book sb ON sb.id = l.sourceBookId
                   JOIN book tb ON tb.id = l.targetBookId
                   JOIN line sl ON sl.id = l.sourceLineId
                   JOIN line tl ON tl.id = l.targetLineId
                  WHERE l.id IN ({placeholders})"
            ))?;
            let endpoint = |r: &rusqlite::Row<'_>, start: usize| -> rusqlite::Result<Endpoint> {
                Ok(Endpoint {
                    book_id: r.get(start)?,
                    title: r.get(start + 1)?,
                    line_id: r.get(start + 2)?,
                    line_index: r.get(start + 3)?,
                    he_ref: r.get(start + 4)?,
                })
            };
            endpoints.extend(
                stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                    Ok((r.get::<_, i64>(0)?, endpoint(r, 1)?, endpoint(r, 6)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        let line_ids: Vec<_> = endpoints
            .iter()
            .flat_map(|(_, source, target)| [source.line_id, target.line_id])
            .collect();
        let texts = self.texts_by_line_id(&mut inner, &line_ids)?;
        Ok(endpoints
            .into_iter()
            .map(|(id, source, target)| {
                let snippet = |line_id| {
                    texts
                        .get(&line_id)
                        .map(|t| text::snippet(t, 160))
                        .unwrap_or_default()
                };
                (
                    id,
                    serde_json::json!({
                        "source_book_id": source.book_id,
                        "source_book_title": source.title,
                        "source_line_index": source.line_index,
                        "source_ref": source.he_ref,
                        "source_snippet": snippet(source.line_id),
                        "target_book_id": target.book_id,
                        "target_book_title": target.title,
                        "target_line_index": target.line_index,
                        "target_ref": target.he_ref,
                        "target_snippet": snippet(target.line_id),
                    }),
                )
            })
            .collect())
    }

    /// Decoded lines `[start, end]` of a book: (lineIndex, line.id, heRef, html).
    pub(crate) fn lines_in_range(
        &self,
        inner: &mut Inner,
        book_id: i64,
        start: i64,
        end: i64,
    ) -> Result<Vec<LineRow>> {
        let sql = format!(
            "SELECT l.lineIndex, l.id, l.heRef, {} FROM line l {} \
              WHERE l.bookId = ?1 AND l.lineIndex BETWEEN ?2 AND ?3 ORDER BY l.lineIndex",
            self.content_col(),
            self.content_join()
        );
        let rows: Vec<(i64, i64, Option<String>, Value)> = {
            let mut stmt = inner.conn.prepare(&sql)?;
            let rows = stmt
                .query_map(rusqlite::params![book_id, start, end], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        Ok(rows
            .into_iter()
            .map(|(li, id, he_ref, v)| (li, id, he_ref, inner.codec.text_lossy(&v)))
            .collect())
    }

    /// Era of every book with a generation (cached). Takes the connection lock on first
    /// use, so never call it (or [`Self::era_of`]) while holding [`Self::lock`].
    pub fn eras(&self) -> &HashMap<i64, Era> {
        self.eras.get_or_init(|| {
            let mut map: HashMap<i64, Era> = HashMap::new();
            if !self.caps.book_generation {
                return map;
            }
            let inner = self.lock();
            let Ok(mut stmt) = inner.conn.prepare(
                "SELECT bg.bookId, g.name FROM book_generation bg JOIN generation g ON g.id = bg.generationId",
            ) else {
                return map;
            };
            if let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))) {
                for (book, name) in rows.flatten() {
                    let era = Era::from_generation_name(&name);
                    map.entry(book)
                        .and_modify(|e| *e = (*e).min(era))
                        .or_insert(era);
                }
            }
            map
        })
    }

    pub fn era_of(&self, book_id: i64) -> Era {
        self.eras().get(&book_id).copied().unwrap_or(Era::Other)
    }

    // ── identity / stats ─────────────────────────────────────────────────────
    /// `schema_meta.db_version` (library content version; v32 today). Falls back to the
    /// legacy dev DB's `db_meta`.
    pub fn schema_version(&self) -> Option<i64> {
        self.meta_int("db_version")
    }

    /// `schema_meta.db_schema_version` (table layout version; 6 today).
    pub fn db_schema_version(&self) -> Option<i64> {
        self.meta_int("db_schema_version")
    }

    fn meta_int(&self, key: &str) -> Option<i64> {
        let inner = self.lock();
        inner
            .conn
            .query_row("SELECT value FROM schema_meta WHERE key = ?1", [key], |r| {
                r.get::<_, String>(0)
            })
            .or_else(|_| {
                let legacy = if key == "db_version" {
                    "schema_version_int"
                } else {
                    key
                };
                inner
                    .conn
                    .query_row("SELECT value FROM db_meta WHERE key = ?1", [legacy], |r| {
                        r.get::<_, String>(0)
                    })
            })
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
    }

    pub fn library_stats(&self) -> Result<LibraryStats> {
        let inner = self.lock();
        let count = |sql: &str| -> i64 { inner.conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
        let mut connection_types = Vec::new();
        if let Ok(mut stmt) = inner
            .conn
            .prepare("SELECT name FROM connection_type ORDER BY id")
        {
            if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
                connection_types = rows.flatten().collect();
            }
        }
        Ok(LibraryStats {
            books: count("SELECT COUNT(*) FROM book"),
            categories: count("SELECT COUNT(*) FROM category"),
            // book.totalLines is denormalised; summing it avoids scanning 7M rows.
            lines: count("SELECT COALESCE(SUM(totalLines), 0) FROM book"),
            connection_types,
        })
    }

    // ── resolve_ref ──────────────────────────────────────────────────────────
    /// Resolve a human reference string to candidate (book_id, line_index) locations.
    pub fn resolve_ref(&self, query: &str, limit: i64) -> Result<Vec<RefCandidate>> {
        let index = self.ref_index.get_or_init(|| {
            let inner = self.lock();
            RefIndex::build(&inner.conn)
        });
        let mut inner = self.lock();
        let mut cands = refs::resolve(&inner.conn, &self.caps, index, query, limit)?;
        // Fill snippets with the decoded text of each candidate line.
        for c in cands.iter_mut() {
            if c.snippet.is_empty() {
                let lines =
                    self.lines_in_range(&mut inner, c.book_id, c.line_index, c.line_index)?;
                if let Some((_, _, _, html)) = lines.first() {
                    c.snippet = text::snippet(html, 160);
                }
            }
        }
        Ok(cands)
    }

    // ── search_books ─────────────────────────────────────────────────────────
    /// Books by title / heRef / acronym / short description, best matches first.
    pub fn search_books(
        &self,
        query: &str,
        category_id: Option<i64>,
        limit: i64,
    ) -> Result<Vec<BookHit>> {
        let like = format!("%{}%", escape_like(query));
        let prefix = format!("{}%", escape_like(query));
        let category_filter = match (category_id, self.caps.category_closure) {
            (Some(_), true) => {
                " AND b.categoryId IN (SELECT descendantId FROM category_closure WHERE ancestorId = ?4)"
            }
            (Some(_), false) => " AND b.categoryId = ?4",
            (None, _) => " AND ?4 IS NULL",
        };
        let sql = format!(
            "SELECT b.id, b.title, b.heRef, c.id, c.title, b.totalLines, b.isBaseBook, b.heShortDesc,
                    (SELECT group_concat(a.name, ', ')
                       FROM book_author ba JOIN author a ON ba.authorId = a.id
                      WHERE ba.bookId = b.id) AS authors,
                    CASE WHEN b.title = ?1 OR b.heRef = ?1 THEN 0
                         WHEN EXISTS (SELECT 1 FROM book_acronym x WHERE x.bookId = b.id AND x.term = ?1) THEN 1
                         WHEN b.title LIKE ?3 ESCAPE '\\' THEN 2
                         WHEN b.title LIKE ?2 ESCAPE '\\' THEN 3
                         WHEN EXISTS (SELECT 1 FROM book_acronym x WHERE x.bookId = b.id AND x.term LIKE ?2 ESCAPE '\\') THEN 4
                         ELSE 5 END AS rank
               FROM book b
               JOIN category c ON b.categoryId = c.id
              WHERE (b.title LIKE ?2 ESCAPE '\\' OR b.heRef LIKE ?2 ESCAPE '\\'
                     OR b.heShortDesc LIKE ?2 ESCAPE '\\'
                     OR EXISTS (SELECT 1 FROM book_acronym x WHERE x.bookId = b.id AND x.term LIKE ?2 ESCAPE '\\'))
                    {category_filter}
              ORDER BY rank, b.isBaseBook DESC, b.orderIndex, b.title
              LIMIT ?5"
        );
        let inner = self.lock();
        let mut stmt = inner.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                rusqlite::params![query, like, prefix, category_id, limit],
                |r| {
                    let short: Option<String> = r.get(7)?;
                    Ok(BookHit {
                        book_id: r.get(0)?,
                        title: r.get(1)?,
                        he_ref: r.get(2)?,
                        category_id: r.get(3)?,
                        category: r.get(4)?,
                        total_lines: r.get(5)?,
                        is_base_book: r.get::<_, i64>(6)? != 0,
                        short_desc: short.map(|s| text::truncate_text(&s, 160)),
                        authors: r.get(8)?,
                        era: None,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        drop(inner);
        Ok(rows
            .into_iter()
            .map(|mut b| {
                b.era = Some(self.era_of(b.book_id));
                b
            })
            .collect())
    }

    // ── lookup_title (acronym / title / alias) ────────────────────────────────
    /// Resolve a book name, acronym or alias (e.g. `רמב"ם`, `שו"ע או"ח`, `ספר בראשית`)
    /// to books. Exact normalized matches first, then names starting with the term.
    pub fn lookup_title(&self, term: &str, limit: i64) -> Result<Vec<TitleMatch>> {
        let norm = refs::normalize_ref(term);
        if norm.is_empty() {
            return Ok(vec![]);
        }
        let index = self.ref_index.get_or_init(|| {
            let inner = self.lock();
            RefIndex::build(&inner.conn)
        });
        let mut out: Vec<TitleMatch> = Vec::new();
        let mut seen = HashSet::new();
        for (name, ids, exact) in index.names_matching(&norm, (limit * 4) as usize) {
            for id in ids {
                if seen.insert(id) {
                    out.push(TitleMatch {
                        book_id: id,
                        title: index.title(id),
                        matched_name: name.clone(),
                        match_kind: if exact { "exact" } else { "prefix" },
                        is_base_book: index.is_base(id),
                        era: self.era_of(id),
                    });
                }
            }
        }
        out.sort_by_key(|m| {
            (
                m.match_kind != "exact",
                !m.is_base_book,
                index.order(m.book_id),
                m.book_id,
            )
        });
        out.truncate(limit as usize);
        Ok(out)
    }

    // ── get_text ─────────────────────────────────────────────────────────────
    pub fn get_text(
        &self,
        book_id: i64,
        line_index: i64,
        limit: i64,
        strip_html: bool,
        version: Option<&str>,
    ) -> Result<Option<GetTextResult>> {
        let mut inner = self.lock();
        let book: Option<(String, i64)> = inner
            .conn
            .query_row(
                "SELECT title, totalLines FROM book WHERE id = ?1",
                [book_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((book_title, total_lines)) = book else {
            return Ok(None);
        };

        let (sql, version_info) = match version {
            None => (
                format!(
                    "SELECT l.lineIndex, l.heRef, {} FROM line l {} \
                      WHERE l.bookId = ?1 AND l.lineIndex >= ?2 ORDER BY l.lineIndex LIMIT ?3",
                    self.content_col(),
                    self.content_join()
                ),
                None,
            ),
            Some(v) => {
                if !(self.caps.book_versions && self.caps.version_line) {
                    anyhow::bail!(
                        "this seforim.db has no text versions (book_version/version_line)"
                    );
                }
                let found: Option<(i64, String, Option<String>)> = inner
                    .conn
                    .query_row(
                        "SELECT id, versionTitle, heVersionTitle FROM book_version \
                          WHERE bookId = ?1 AND (versionTitle = ?2 OR heVersionTitle = ?2) LIMIT 1",
                        rusqlite::params![book_id, v],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;
                let Some((vid, title, he_title)) = found else {
                    anyhow::bail!(
                        "book {book_id} has no version titled {v:?} (see get_book_info.versions)"
                    );
                };
                // As the app: structural lines (heRef NULL — headings) come from the base
                // text; a content line takes the edition's text, and a segment missing from
                // the edition is shown empty (never silently the merged text).
                let case = if self.caps.split_content {
                    "CASE WHEN l.heRef IS NULL THEN lc.content \
                          WHEN vl.lineId IS NULL THEN '' \
                          ELSE COALESCE(vl.content, lc.content, '') END"
                } else {
                    "CASE WHEN l.heRef IS NULL THEN l.content ELSE COALESCE(vl.content, '') END"
                };
                (
                    format!(
                        "SELECT l.lineIndex, l.heRef, {case} FROM line l {} \
                           LEFT JOIN version_line vl ON vl.versionId = {vid} AND vl.lineId = l.id \
                          WHERE l.bookId = ?1 AND l.lineIndex >= ?2 ORDER BY l.lineIndex LIMIT ?3",
                        self.content_join()
                    ),
                    Some(VersionRef {
                        version_title: title,
                        he_version_title: he_title,
                    }),
                )
            }
        };

        let raw: Vec<(i64, Option<String>, Value)> = {
            let mut stmt = inner.conn.prepare(&sql)?;
            let rows = stmt
                .query_map(rusqlite::params![book_id, line_index, limit], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let lines: Vec<TextLine> = raw
            .into_iter()
            .map(|(li, he_ref, v)| {
                let html = inner.codec.text_lossy(&v);
                TextLine {
                    line_index: li,
                    he_ref,
                    content: if strip_html {
                        strip_html_tags(&html)
                    } else {
                        html
                    },
                }
            })
            .collect();

        let next_line_index = match lines.last() {
            Some(last) if (lines.len() as i64) == limit && last.line_index + 1 < total_lines => {
                Some(last.line_index + 1)
            }
            _ => None,
        };

        Ok(Some(GetTextResult {
            book_id,
            book_title,
            total_lines,
            version: version_info,
            lines,
            next_line_index,
        }))
    }

    // ── get_book_info ────────────────────────────────────────────────────────
    pub fn get_book_info(&self, book_id: i64) -> Result<Option<BookInfo>> {
        let dep_cols = if self.caps.book_dependence {
            "b.dependenceType, b.collectiveTitleHe, b.collectiveTitleEn"
        } else {
            "NULL, NULL, NULL"
        };
        let has_links = if self.caps.book_has_links {
            "(SELECT hasSourceLinks FROM book_has_links WHERE bookId = b.id), \
             (SELECT hasTargetLinks FROM book_has_links WHERE bookId = b.id)"
        } else {
            "NULL, NULL"
        };
        let sql = format!(
            "SELECT b.id, b.title, b.heRef, c.id, c.title, b.heShortDesc, b.heDesc, s.name,
                    b.totalLines, b.isBaseBook,
                    b.hasCommentaryConnection, b.hasTargumConnection, b.hasReferenceConnection,
                    b.hasSourceConnection, b.hasOtherConnection,
                    b.hasAltStructures, b.hasTeamim, b.hasNekudot, b.orderIndex,
                    {dep_cols}, {has_links}
               FROM book b
               JOIN category c ON b.categoryId = c.id
               LEFT JOIN source s ON b.sourceId = s.id
              WHERE b.id = ?1"
        );
        let base = {
            let inner = self.lock();
            inner
                .conn
                .query_row(&sql, [book_id], |r| {
                    let flag =
                        |i: usize| -> rusqlite::Result<bool> { Ok(r.get::<_, i64>(i)? != 0) };
                    let opt_flag = |i: usize| -> rusqlite::Result<Option<bool>> {
                        Ok(r.get::<_, Option<i64>>(i)?.map(|v| v != 0))
                    };
                    Ok(BookInfo {
                        book_id: r.get(0)?,
                        title: r.get(1)?,
                        he_ref: r.get(2)?,
                        category_id: r.get(3)?,
                        category: r.get(4)?,
                        category_path: Vec::new(),
                        short_desc: r.get(5)?,
                        long_desc: r.get(6)?,
                        source: r.get(7)?,
                        total_lines: r.get(8)?,
                        is_base_book: flag(9)?,
                        connections: Connections {
                            commentary: flag(10)?,
                            targum: flag(11)?,
                            reference: flag(12)?,
                            source: flag(13)?,
                            other: flag(14)?,
                            has_outgoing_links: opt_flag(22)?,
                            has_incoming_links: opt_flag(23)?,
                        },
                        has_alt_structures: flag(15)?,
                        has_teamim: flag(16)?,
                        has_nekudot: flag(17)?,
                        order_index: r.get(18)?,
                        dependence_type: r.get(19)?,
                        collective_title: r.get(20)?,
                        collective_title_en: r.get(21)?,
                        authors: Vec::new(),
                        generations: Vec::new(),
                        era: Era::Other,
                        pub_dates: Vec::new(),
                        pub_places: Vec::new(),
                        topics: Vec::new(),
                        acronyms: Vec::new(),
                        base_texts: Vec::new(),
                        default_commentators: Vec::new(),
                        default_targumim: Vec::new(),
                        alt_structures: Vec::new(),
                        versions: Vec::new(),
                    })
                })
                .optional()?
        };
        let Some(mut info) = base else {
            return Ok(None);
        };
        info.era = self.era_of(book_id);
        info.category_path = self.category_path(info.category_id)?;

        let inner = self.lock();
        let strings = |sql: &str| -> Result<Vec<String>> {
            let mut stmt = inner.conn.prepare(sql)?;
            let v = stmt
                .query_map([book_id], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(v)
        };
        let books = |sql: &str| -> Result<Vec<NamedBook>> {
            let mut stmt = inner.conn.prepare(sql)?;
            let v = stmt
                .query_map([book_id], |r| {
                    Ok(NamedBook {
                        book_id: r.get(0)?,
                        title: r.get(1)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(v)
        };

        info.authors = strings(
            "SELECT a.name FROM book_author ba JOIN author a ON ba.authorId = a.id WHERE ba.bookId = ?1",
        )?;
        if self.caps.book_generation {
            info.generations = strings(
                "SELECT g.name FROM book_generation bg JOIN generation g ON bg.generationId = g.id WHERE bg.bookId = ?1",
            )?;
        }
        info.topics = strings(
            "SELECT t.name FROM book_topic bt JOIN topic t ON bt.topicId = t.id WHERE bt.bookId = ?1",
        )
        .unwrap_or_default();
        info.acronyms =
            strings("SELECT term FROM book_acronym WHERE bookId = ?1 ORDER BY term LIMIT 60")?;
        if self.caps.pub_dates {
            info.pub_dates = strings(
                "SELECT d.date FROM book_pub_date bd JOIN pub_date d ON d.id = bd.pubDateId WHERE bd.bookId = ?1 ORDER BY d.date",
            )?;
        }
        if self.caps.pub_places {
            info.pub_places = strings(
                "SELECT p.name FROM book_pub_place bp JOIN pub_place p ON p.id = bp.pubPlaceId WHERE bp.bookId = ?1",
            )?;
        }
        if self.caps.book_base_text {
            info.base_texts = books(
                "SELECT b.id, b.title FROM book_base_text bt JOIN book b ON b.id = bt.baseBookId WHERE bt.bookId = ?1 ORDER BY b.orderIndex",
            )?;
        }
        info.default_commentators = books(
            "SELECT b.id, b.title FROM default_commentator dc JOIN book b ON b.id = dc.commentatorBookId WHERE dc.bookId = ?1 ORDER BY dc.position",
        )?;
        if self.caps.default_targum {
            info.default_targumim = books(
                "SELECT b.id, b.title FROM default_targum dt JOIN book b ON b.id = dt.targumBookId WHERE dt.bookId = ?1 ORDER BY dt.position",
            )?;
        }
        if self.caps.alt_toc {
            let mut stmt = inner.conn.prepare(
                "SELECT key, COALESCE(heTitle, title, key) FROM alt_toc_structure WHERE bookId = ?1 ORDER BY id",
            )?;
            info.alt_structures = stmt
                .query_map([book_id], |r| {
                    Ok(AltStructure {
                        key: r.get(0)?,
                        title: r.get(1)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        if self.caps.book_versions {
            let mut stmt = inner.conn.prepare(
                "SELECT versionTitle, heVersionTitle, versionSource, license, priority, hasContent,
                        COALESCE(heVersionNotes, versionNotes)
                   FROM book_version WHERE bookId = ?1
                  ORDER BY hasContent DESC, priority DESC, versionTitle",
            )?;
            info.versions = stmt
                .query_map([book_id], |r| {
                    let notes: Option<String> = r.get(6)?;
                    Ok(BookVersion {
                        version_title: r.get(0)?,
                        he_version_title: r.get(1)?,
                        source: r.get(2)?,
                        license: r.get(3)?,
                        priority: r.get(4)?,
                        has_text: r.get::<_, i64>(5)? != 0,
                        notes: notes.map(|n| text::truncate_text(&strip_html_tags(&n), 300)),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        Ok(Some(info))
    }

    /// Category titles from the root down to `category_id`.
    pub fn category_path(&self, category_id: i64) -> Result<Vec<NamedCategory>> {
        let inner = self.lock();
        let mut path = Vec::new();
        let mut current = Some(category_id);
        let mut guard = 0;
        while let Some(id) = current {
            guard += 1;
            if guard > 64 {
                break;
            }
            let row: Option<(String, Option<i64>)> = inner
                .conn
                .query_row(
                    "SELECT title, parentId FROM category WHERE id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((title, parent)) = row else { break };
            path.push(NamedCategory {
                category_id: id,
                title,
            });
            current = parent;
        }
        path.reverse();
        Ok(path)
    }

    /// The search-index facet path of a category: `/<root title>/…/<title>`.
    pub fn category_facet(&self, category_id: i64) -> Result<Option<String>> {
        let path = self.category_path(category_id)?;
        if path.is_empty() {
            return Ok(None);
        }
        Ok(Some(format!(
            "/{}",
            path.iter()
                .map(|c| c.title.as_str())
                .collect::<Vec<_>>()
                .join("/")
        )))
    }

    /// Titles of the given book ids.
    pub fn book_titles(&self, ids: &[i64]) -> Result<HashMap<i64, String>> {
        let inner = self.lock();
        let mut out = HashMap::new();
        for chunk in ids.chunks(400) {
            let ph = vec!["?"; chunk.len()].join(",");
            let mut stmt = inner
                .conn
                .prepare(&format!("SELECT id, title FROM book WHERE id IN ({ph})"))?;
            for row in stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })? {
                let (id, t) = row?;
                out.insert(id, t);
            }
        }
        Ok(out)
    }

    // ── get_book_toc ─────────────────────────────────────────────────────────
    /// Flattened (pre-order) table of contents with line_index pointers for get_text,
    /// plus the book's alternate structures. `parent_id` narrows to one node's children.
    pub fn get_book_toc(
        &self,
        book_id: i64,
        limit: i64,
        alt_structure_key: Option<&str>,
        parent_id: Option<i64>,
    ) -> Result<BookToc> {
        let inner = self.lock();
        let parent_filter = if parent_id.is_some() {
            "AND te.parentId = ?3"
        } else {
            "AND ?3 IS NULL"
        };
        let mut stmt = inner.conn.prepare(&format!(
            "SELECT te.id, te.parentId, te.level, tt.text, te.hasChildren, l.lineIndex \
               FROM tocEntry te \
               JOIN tocText tt ON te.textId = tt.id \
               LEFT JOIN line l ON te.lineId = l.id \
              WHERE te.bookId = ?1 {parent_filter} ORDER BY te.id LIMIT ?2"
        ))?;
        let entries = stmt
            .query_map(rusqlite::params![book_id, limit + 1, parent_id], toc_node)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let truncated = entries.len() as i64 > limit;
        let entries: Vec<TocNode> = entries.into_iter().take(limit as usize).collect();

        let mut alt_structures = Vec::new();
        let mut selected_alt_structure = None;
        let mut alt_entries = Vec::new();
        let mut alt_truncated = false;
        if self.caps.alt_toc {
            let mut stmt = inner.conn.prepare(
                "SELECT key, COALESCE(heTitle, title, key) FROM alt_toc_structure WHERE bookId = ?1 ORDER BY id",
            )?;
            alt_structures = stmt
                .query_map([book_id], |r| {
                    Ok(AltStructure {
                        key: r.get(0)?,
                        title: r.get(1)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            if let Some(key) = alt_structure_key {
                let selected = inner
                    .conn
                    .query_row(
                        "SELECT id, key, COALESCE(heTitle, title, key) \
                           FROM alt_toc_structure WHERE bookId = ?1 AND key = ?2",
                        rusqlite::params![book_id, key],
                        |r| {
                            Ok((
                                r.get::<_, i64>(0)?,
                                AltStructure {
                                    key: r.get(1)?,
                                    title: r.get(2)?,
                                },
                            ))
                        },
                    )
                    .optional()?;
                if let Some((structure_id, structure)) = selected {
                    let parent_filter = if parent_id.is_some() {
                        "AND ae.parentId = ?3"
                    } else {
                        "AND ?3 IS NULL"
                    };
                    let mut stmt = inner.conn.prepare(&format!(
                        "SELECT ae.id, ae.parentId, ae.level, tt.text, ae.hasChildren, l.lineIndex \
                           FROM alt_toc_entry ae \
                           JOIN tocText tt ON ae.textId = tt.id \
                           LEFT JOIN line l ON ae.lineId = l.id \
                          WHERE ae.structureId = ?1 {parent_filter} ORDER BY ae.id LIMIT ?2"
                    ))?;
                    let nodes = stmt
                        .query_map(
                            rusqlite::params![structure_id, limit + 1, parent_id],
                            toc_node,
                        )?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    alt_truncated = nodes.len() as i64 > limit;
                    alt_entries = nodes.into_iter().take(limit as usize).collect();
                    selected_alt_structure = Some(structure);
                } else {
                    anyhow::bail!(
                        "book {book_id} has no alternate structure {key:?} (see alt_structures)"
                    );
                }
            }
        }

        Ok(BookToc {
            entries,
            truncated,
            alt_structures,
            selected_alt_structure,
            alt_entries,
            alt_truncated,
        })
    }

    // ── browse_categories ────────────────────────────────────────────────────
    /// Navigate the category tree: child categories (with recursive book counts) and the
    /// books directly under a category. With no category, returns the root categories.
    pub fn browse_categories(
        &self,
        category_id: Option<i64>,
        path: Option<&str>,
        limit: i64,
    ) -> Result<CategoryView> {
        let category_id = match (category_id, path) {
            (Some(id), _) => Some(id),
            (None, Some(p)) if !p.trim().trim_matches('/').is_empty() => {
                Some(self.category_by_path(p)?.with_context(|| {
                    format!("no category at path {p:?} (titles separated by '/')")
                })?)
            }
            _ => None,
        };
        let current = match category_id {
            Some(id) => {
                let desc_cols = if self.caps.category_descriptions {
                    "heShortDesc, heDesc"
                } else {
                    "NULL, NULL"
                };
                let row = {
                    let inner = self.lock();
                    inner
                        .conn
                        .query_row(
                            &format!("SELECT id, title, {desc_cols} FROM category WHERE id = ?1"),
                            [id],
                            |r| {
                                Ok(CurrentCategory {
                                    category_id: r.get(0)?,
                                    title: r.get(1)?,
                                    short_desc: r.get(2)?,
                                    long_desc: r.get(3)?,
                                    path: Vec::new(),
                                })
                            },
                        )
                        .optional()?
                };
                let Some(mut c) = row else {
                    anyhow::bail!("no category with category_id={id}");
                };
                c.path = self.category_path(id)?;
                Some(c)
            }
            None => None,
        };

        let inner = self.lock();
        let book_count = if self.caps.category_closure {
            "(SELECT COUNT(*) FROM book b JOIN category_closure cc ON cc.descendantId = b.categoryId WHERE cc.ancestorId = c.id)"
        } else {
            "(SELECT COUNT(*) FROM book b WHERE b.categoryId = c.id)"
        };
        let short = if self.caps.category_descriptions {
            "c.heShortDesc"
        } else {
            "NULL"
        };
        let where_parent = if category_id.is_some() {
            "c.parentId = ?1"
        } else {
            "c.parentId IS NULL AND ?1 IS NULL"
        };
        let mut stmt = inner.conn.prepare(&format!(
            "SELECT c.id, c.title, EXISTS(SELECT 1 FROM category cc WHERE cc.parentId = c.id), {book_count}, {short} \
               FROM category c WHERE {where_parent} ORDER BY c.orderIndex, c.title"
        ))?;
        let subcategories = stmt
            .query_map([category_id], |r| {
                Ok(SubCategory {
                    category_id: r.get(0)?,
                    title: r.get(1)?,
                    has_children: r.get::<_, i64>(2)? != 0,
                    book_count: r.get(3)?,
                    short_desc: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let books = match category_id {
            Some(id) => {
                let mut stmt = inner.conn.prepare(
                    "SELECT id, title, heRef, isBaseBook, totalLines FROM book \
                      WHERE categoryId = ?1 ORDER BY orderIndex, title LIMIT ?2",
                )?;
                let v = stmt
                    .query_map(rusqlite::params![id, limit], |r| {
                        Ok(CategoryBook {
                            book_id: r.get(0)?,
                            title: r.get(1)?,
                            he_ref: r.get(2)?,
                            is_base_book: r.get::<_, i64>(3)? != 0,
                            total_lines: r.get(4)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                v
            }
            None => Vec::new(),
        };

        Ok(CategoryView {
            current,
            subcategories,
            books,
        })
    }

    /// Find a category by its title path (`תנ"ך/תורה`), ignoring gershayim style.
    pub fn category_by_path(&self, path: &str) -> Result<Option<i64>> {
        let norm = |s: &str| refs::normalize_ref(s);
        let parts: Vec<String> = path
            .split('/')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(norm)
            .collect();
        let inner = self.lock();
        let mut parent: Option<i64> = None;
        for part in parts {
            let mut stmt = inner.conn.prepare(
                "SELECT id, title FROM category WHERE (parentId = ?1 OR (?1 IS NULL AND parentId IS NULL))",
            )?;
            let children = stmt
                .query_map([parent], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            match children.into_iter().find(|(_, t)| norm(t) == part) {
                Some((id, _)) => parent = Some(id),
                None => return Ok(None),
            }
        }
        Ok(parent)
    }
}

fn toc_node(r: &rusqlite::Row) -> rusqlite::Result<TocNode> {
    Ok(TocNode {
        id: r.get(0)?,
        parent_id: r.get(1)?,
        level: r.get(2)?,
        text: r.get(3)?,
        has_children: r.get::<_, i64>(4)? != 0,
        line_index: r.get(5)?,
    })
}

// ── result types ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct LibraryStats {
    pub books: i64,
    pub categories: i64,
    pub lines: i64,
    pub connection_types: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct BookHit {
    pub book_id: i64,
    pub title: String,
    pub he_ref: Option<String>,
    pub category_id: i64,
    pub category: String,
    pub authors: Option<String>,
    pub era: Option<Era>,
    pub total_lines: i64,
    pub is_base_book: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_desc: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TitleMatch {
    pub book_id: i64,
    pub title: String,
    /// The normalized name/alias that matched (title, heRef or a `book_acronym` term).
    pub matched_name: String,
    /// "exact" (the whole normalized term) or "prefix" (a name starting with it).
    pub match_kind: &'static str,
    pub is_base_book: bool,
    pub era: Era,
}

#[derive(Debug, Serialize)]
pub struct TextLine {
    pub line_index: i64,
    pub he_ref: Option<String>,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct VersionRef {
    pub version_title: String,
    pub he_version_title: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GetTextResult {
    pub book_id: i64,
    pub book_title: String,
    pub total_lines: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<VersionRef>,
    pub lines: Vec<TextLine>,
    pub next_line_index: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct Connections {
    pub commentary: bool,
    pub targum: bool,
    pub reference: bool,
    pub source: bool,
    pub other: bool,
    /// `book_has_links.hasSourceLinks` (the book is the stored source of links).
    pub has_outgoing_links: Option<bool>,
    /// `book_has_links.hasTargetLinks`.
    pub has_incoming_links: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct BookVersion {
    pub version_title: String,
    pub he_version_title: Option<String>,
    pub source: Option<String>,
    pub license: Option<String>,
    pub priority: Option<f64>,
    /// The edition's own text is stored (selectable with get_text `version`).
    pub has_text: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BookInfo {
    pub book_id: i64,
    pub title: String,
    pub he_ref: Option<String>,
    pub category_id: i64,
    pub category: String,
    pub category_path: Vec<NamedCategory>,
    pub short_desc: Option<String>,
    pub long_desc: Option<String>,
    pub source: Option<String>,
    pub total_lines: i64,
    pub order_index: i64,
    pub is_base_book: bool,
    /// 'Commentary' | 'Targum' | 'Midrash' | 'Guides' (NULL for independent works).
    pub dependence_type: Option<String>,
    /// Collective title (e.g. `רש"י` for every "רש"י על …").
    pub collective_title: Option<String>,
    pub collective_title_en: Option<String>,
    pub connections: Connections,
    pub has_alt_structures: bool,
    pub has_teamim: bool,
    pub has_nekudot: bool,
    pub authors: Vec<String>,
    pub generations: Vec<String>,
    pub era: Era,
    pub pub_dates: Vec<String>,
    pub pub_places: Vec<String>,
    pub topics: Vec<String>,
    pub acronyms: Vec<String>,
    /// Books this one depends on / comments on (`book_base_text`).
    pub base_texts: Vec<NamedBook>,
    pub default_commentators: Vec<NamedBook>,
    pub default_targumim: Vec<NamedBook>,
    pub alt_structures: Vec<AltStructure>,
    pub versions: Vec<BookVersion>,
}

#[derive(Debug, Serialize, Clone)]
pub struct NamedBook {
    pub book_id: i64,
    pub title: String,
}

#[derive(Debug, Serialize)]
pub struct TocNode {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub level: i64,
    pub text: String,
    pub has_children: bool,
    /// Pass as get_text's line_index (null for structural nodes without a line).
    pub line_index: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct AltStructure {
    pub key: String,
    pub title: String,
}

#[derive(Debug, Serialize)]
pub struct BookToc {
    pub entries: Vec<TocNode>,
    pub truncated: bool,
    pub alt_structures: Vec<AltStructure>,
    pub selected_alt_structure: Option<AltStructure>,
    pub alt_entries: Vec<TocNode>,
    pub alt_truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct NamedCategory {
    pub category_id: i64,
    pub title: String,
}

#[derive(Debug, Serialize)]
pub struct CurrentCategory {
    pub category_id: i64,
    pub title: String,
    pub short_desc: Option<String>,
    pub long_desc: Option<String>,
    pub path: Vec<NamedCategory>,
}

#[derive(Debug, Serialize)]
pub struct SubCategory {
    pub category_id: i64,
    pub title: String,
    pub has_children: bool,
    /// Books in this category and all its descendants.
    pub book_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_desc: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CategoryBook {
    pub book_id: i64,
    pub title: String,
    pub he_ref: Option<String>,
    pub is_base_book: bool,
    pub total_lines: i64,
}

#[derive(Debug, Serialize)]
pub struct CategoryView {
    pub current: Option<CurrentCategory>,
    pub subcategories: Vec<SubCategory>,
    pub books: Vec<CategoryBook>,
}

/// Escape `%` and `_` (and the `\` escape char) for a SQL LIKE pattern.
pub(crate) fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}
