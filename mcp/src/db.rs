//! Read-only access to Otzaria's `seforim.db` (schema v15). Every query is
//! read-only; the DB is opened with `SQLITE_OPEN_READ_ONLY` (WAL-aware) by
//! default, or `immutable=1` only in explicit `--snapshot` mode.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use std::sync::{Mutex, OnceLock};

use crate::paths::Paths;
use crate::refs::{self, RefCandidate, RefIndex};

pub struct Db {
    conn: Mutex<Connection>,
    /// Lazily-built normalized book-name index for resolve_ref (find_ref cache).
    ref_index: OnceLock<RefIndex>,
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

        conn.execute_batch("PRAGMA query_only=ON;")
            .context("setting query_only pragma")?;

        Ok(Db {
            conn: Mutex::new(conn),
            ref_index: OnceLock::new(),
        })
    }

    // ── resolve_ref ──────────────────────────────────────────────────────────
    /// Resolve a human reference string to candidate (book_id, line_index) locations.
    pub fn resolve_ref(&self, query: &str, limit: i64) -> Result<Vec<RefCandidate>> {
        let conn = self.conn.lock().unwrap();
        let index = self.ref_index.get_or_init(|| RefIndex::build(&conn));
        refs::resolve(&conn, index, query, limit)
    }

    /// `schema_meta.db_version` (schema v15 uses `schema_meta`; the legacy dev DB
    /// used `db_meta` — fall back to it so both open cleanly).
    pub fn schema_version(&self) -> Option<i64> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT value FROM schema_meta WHERE key='db_version'",
            [],
            |r| r.get::<_, String>(0),
        )
        .or_else(|_| {
            conn.query_row(
                "SELECT value FROM db_meta WHERE key='schema_version_int'",
                [],
                |r| r.get::<_, String>(0),
            )
        })
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
    }

    // ── search_books ─────────────────────────────────────────────────────────
    pub fn search_books(&self, query: &str, limit: i64) -> Result<Vec<BookHit>> {
        let like = format!("%{}%", escape_like(query));
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT b.id, b.title, b.heRef, c.title AS category, b.totalLines, b.isBaseBook,
                    (SELECT group_concat(a.name, ', ')
                       FROM book_author ba JOIN author a ON ba.authorId = a.id
                      WHERE ba.bookId = b.id) AS authors
               FROM book b
               JOIN category c ON b.categoryId = c.id
              WHERE b.title LIKE ?1 ESCAPE '\\' OR b.heShortDesc LIKE ?1 ESCAPE '\\'
              ORDER BY b.isBaseBook DESC, b.orderIndex, b.title
              LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![like, limit], |r| {
            Ok(BookHit {
                book_id: r.get(0)?,
                title: r.get(1)?,
                he_ref: r.get(2)?,
                category: r.get(3)?,
                total_lines: r.get(4)?,
                is_base_book: r.get::<_, i64>(5)? != 0,
                authors: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ── get_text ─────────────────────────────────────────────────────────────
    pub fn get_text(
        &self,
        book_id: i64,
        line_index: i64,
        limit: i64,
        strip_html: bool,
    ) -> Result<Option<GetTextResult>> {
        let conn = self.conn.lock().unwrap();
        let book_title: Option<String> = conn
            .query_row("SELECT title FROM book WHERE id = ?1", [book_id], |r| {
                r.get(0)
            })
            .ok();
        let Some(book_title) = book_title else {
            return Ok(None);
        };

        let mut stmt = conn.prepare(
            "SELECT lineIndex, heRef, content
               FROM line
              WHERE bookId = ?1 AND lineIndex >= ?2
              ORDER BY lineIndex
              LIMIT ?3",
        )?;
        let lines = stmt
            .query_map(rusqlite::params![book_id, line_index, limit], |r| {
                let raw: String = r.get(2)?;
                Ok(TextLine {
                    line_index: r.get(0)?,
                    he_ref: r.get(1)?,
                    content: if strip_html {
                        strip_html_tags(&raw)
                    } else {
                        raw
                    },
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let next_line_index = if (lines.len() as i64) == limit {
            lines.last().map(|l| l.line_index + 1)
        } else {
            None
        };

        Ok(Some(GetTextResult {
            book_id,
            book_title,
            lines,
            next_line_index,
        }))
    }

    // ── get_links ────────────────────────────────────────────────────────────
    /// Cross-references for a book (optionally a single line). `direction`:
    /// "outgoing" (book is the link source), "incoming" (book is the target), or
    /// "both". Optional `connection_type` filters by name (e.g. "COMMENTARY").
    pub fn get_links(
        &self,
        book_id: i64,
        line_index: Option<i64>,
        connection_type: Option<&str>,
        direction: &str,
        limit: i64,
    ) -> Result<Vec<LinkRow>> {
        let conn = self.conn.lock().unwrap();
        let want_out = matches!(direction, "outgoing" | "both");
        let want_in = matches!(direction, "incoming" | "both");
        if !want_out && !want_in {
            anyhow::bail!("direction must be one of: outgoing | incoming | both");
        }
        let mut rows = Vec::new();
        if want_out {
            rows.extend(query_links(
                &conn,
                book_id,
                line_index,
                connection_type,
                true,
                limit,
            )?);
        }
        if want_in {
            let remaining = limit - rows.len() as i64;
            if remaining > 0 {
                rows.extend(query_links(
                    &conn,
                    book_id,
                    line_index,
                    connection_type,
                    false,
                    remaining,
                )?);
            }
        }
        Ok(rows)
    }

    // ── get_book_info ────────────────────────────────────────────────────────
    pub fn get_book_info(&self, book_id: i64) -> Result<Option<BookInfo>> {
        let conn = self.conn.lock().unwrap();
        let base = conn
            .query_row(
                "SELECT b.id, b.title, b.heRef, c.title, b.heShortDesc, b.heDesc, s.name,
                        b.totalLines, b.isBaseBook,
                        b.hasCommentaryConnection, b.hasTargumConnection, b.hasReferenceConnection,
                        b.hasSourceConnection, b.hasOtherConnection,
                        b.hasAltStructures, b.hasTeamim, b.hasNekudot
                   FROM book b
                   JOIN category c ON b.categoryId = c.id
                   JOIN source s ON b.sourceId = s.id
                  WHERE b.id = ?1",
                [book_id],
                |r| {
                    Ok(BookInfo {
                        book_id: r.get(0)?,
                        title: r.get(1)?,
                        he_ref: r.get(2)?,
                        category: r.get(3)?,
                        short_desc: r.get(4)?,
                        long_desc: r.get(5)?,
                        source: r.get(6)?,
                        total_lines: r.get(7)?,
                        is_base_book: r.get::<_, i64>(8)? != 0,
                        connections: Connections {
                            commentary: r.get::<_, i64>(9)? != 0,
                            targum: r.get::<_, i64>(10)? != 0,
                            reference: r.get::<_, i64>(11)? != 0,
                            source: r.get::<_, i64>(12)? != 0,
                            other: r.get::<_, i64>(13)? != 0,
                        },
                        has_alt_structures: r.get::<_, i64>(14)? != 0,
                        has_teamim: r.get::<_, i64>(15)? != 0,
                        has_nekudot: r.get::<_, i64>(16)? != 0,
                        authors: Vec::new(),
                        generations: Vec::new(),
                        topics: Vec::new(),
                        acronyms: Vec::new(),
                        versions: Vec::new(),
                    })
                },
            )
            .ok();
        let Some(mut info) = base else {
            return Ok(None);
        };

        // authors (v15: `author` has no generationId; generation is a book-level m2m)
        let mut stmt = conn.prepare(
            "SELECT a.name FROM book_author ba JOIN author a ON ba.authorId = a.id WHERE ba.bookId = ?1",
        )?;
        info.authors = stmt
            .query_map([book_id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // historical generation(s)/era, via book_generation
        let mut stmt = conn.prepare(
            "SELECT g.name FROM book_generation bg JOIN generation g ON bg.generationId = g.id WHERE bg.bookId = ?1",
        )?;
        info.generations = stmt
            .query_map([book_id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // topics
        let mut stmt = conn.prepare(
            "SELECT t.name FROM book_topic bt JOIN topic t ON bt.topicId = t.id WHERE bt.bookId = ?1",
        )?;
        info.topics = stmt
            .query_map([book_id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // a few acronyms/aliases
        let mut stmt = conn.prepare("SELECT term FROM book_acronym WHERE bookId = ?1 LIMIT 12")?;
        info.acronyms = stmt
            .query_map([book_id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // text versions (may be absent on the legacy dev DB — ignore errors)
        if let Ok(mut stmt) = conn.prepare(
            "SELECT COALESCE(heVersionTitle, versionTitle) FROM book_version WHERE bookId = ?1 ORDER BY priority DESC",
        ) {
            if let Ok(rows) = stmt.query_map([book_id], |r| r.get::<_, String>(0)) {
                info.versions = rows.filter_map(|r| r.ok()).collect();
            }
        }

        Ok(Some(info))
    }

    // ── get_commentaries ─────────────────────────────────────────────────────
    /// Commentaries/targumim on a book's line range: the actual commentary text
    /// (target lines) plus the book's ordered default-commentator hint.
    pub fn get_commentaries(
        &self,
        book_id: i64,
        start_line: i64,
        end_line: i64,
        commentator_ids: Option<&[i64]>,
        limit: i64,
    ) -> Result<Commentaries> {
        let conn = self.conn.lock().unwrap();

        let mut sql = String::from(
            "SELECT ct.name, tb.id, tb.title, tl.lineIndex, tl.heRef, tl.content, sl.lineIndex \
               FROM link lk \
               JOIN connection_type ct ON lk.connectionTypeId = ct.id \
               JOIN line sl ON lk.sourceLineId = sl.id \
               JOIN book tb ON lk.targetBookId = tb.id \
               JOIN line tl ON lk.targetLineId = tl.id \
               LEFT JOIN link_range lr ON lr.linkId = lk.id AND lr.side = 0 \
              WHERE lk.sourceBookId = ?1 AND sl.lineIndex <= ?3 \
                AND COALESCE(lr.endLineIndex, sl.lineIndex) >= ?2 \
                AND ct.name IN ('COMMENTARY','SUPER_COMMENTARY','TARGUM','MIDRASH','PARSHANUT','DIBUR_HAMATCHIL')",
        );
        let mut params: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(book_id), Box::new(start_line), Box::new(end_line)];
        if let Some(ids) = commentator_ids {
            if !ids.is_empty() {
                let ph = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                sql.push_str(&format!(" AND tb.id IN ({ph})"));
                for id in ids {
                    params.push(Box::new(*id));
                }
            }
        }
        sql.push_str(" ORDER BY lk.targetBookOrderIndex, tl.lineIndex LIMIT ?");
        params.push(Box::new(limit));

        let mut stmt = conn.prepare(&sql)?;
        let items = stmt
            .query_map(
                rusqlite::params_from_iter(params.iter().map(|b| b.as_ref())),
                |r| {
                    let content: String = r.get(5)?;
                    Ok(CommentaryItem {
                        connection_type: r.get(0)?,
                        commentator_book_id: r.get(1)?,
                        commentator_title: r.get(2)?,
                        on_line_index: r.get(6)?,
                        target_line_index: r.get(3)?,
                        target_ref: r.get(4)?,
                        text: strip_html_tags(&content),
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // Ordered default commentators for this book (a hint / filter menu).
        let mut stmt = conn.prepare(
            "SELECT b.id, b.title FROM default_commentator dc \
               JOIN book b ON dc.commentatorBookId = b.id \
              WHERE dc.bookId = ?1 ORDER BY dc.position",
        )?;
        let defaults = stmt
            .query_map([book_id], |r| {
                Ok(NamedBook {
                    book_id: r.get(0)?,
                    title: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(Commentaries {
            default_commentators: defaults,
            items,
        })
    }

    // ── get_book_toc ─────────────────────────────────────────────────────────
    /// Flattened (pre-order) table of contents with line_index pointers for
    /// get_text, plus the book's alternate structures.
    pub fn get_book_toc(
        &self,
        book_id: i64,
        limit: i64,
        alt_structure_key: Option<&str>,
    ) -> Result<BookToc> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT te.id, te.parentId, te.level, tt.text, te.hasChildren, l.lineIndex \
               FROM tocEntry te \
               JOIN tocText tt ON te.textId = tt.id \
               LEFT JOIN line l ON te.lineId = l.id \
              WHERE te.bookId = ?1 ORDER BY te.id LIMIT ?2",
        )?;
        let entries = stmt
            .query_map(rusqlite::params![book_id, limit + 1], |r| {
                Ok(TocNode {
                    id: r.get(0)?,
                    parent_id: r.get(1)?,
                    level: r.get(2)?,
                    text: r.get(3)?,
                    has_children: r.get::<_, i64>(4)? != 0,
                    line_index: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let truncated = entries.len() as i64 > limit;
        let entries: Vec<TocNode> = entries.into_iter().take(limit as usize).collect();

        let mut stmt = conn.prepare(
            "SELECT key, COALESCE(heTitle, title, key) FROM alt_toc_structure WHERE bookId = ?1",
        )?;
        let alt_structures = stmt
            .query_map([book_id], |r| {
                Ok(AltStructure {
                    key: r.get(0)?,
                    title: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let selected_alt = if let Some(key) = alt_structure_key {
            conn.query_row(
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
            .optional()?
        } else {
            None
        };

        let (selected_alt_structure, alt_entries, alt_truncated) =
            if let Some((structure_id, structure)) = selected_alt {
                let mut stmt = conn.prepare(
                    "SELECT ae.id, ae.parentId, ae.level, tt.text, ae.hasChildren, l.lineIndex \
                       FROM alt_toc_entry ae \
                       JOIN tocText tt ON ae.textId = tt.id \
                       LEFT JOIN line l ON ae.lineId = l.id \
                      WHERE ae.structureId = ?1 ORDER BY ae.id LIMIT ?2",
                )?;
                let nodes = stmt
                    .query_map(rusqlite::params![structure_id, limit + 1], |r| {
                        Ok(TocNode {
                            id: r.get(0)?,
                            parent_id: r.get(1)?,
                            level: r.get(2)?,
                            text: r.get(3)?,
                            has_children: r.get::<_, i64>(4)? != 0,
                            line_index: r.get(5)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let truncated = nodes.len() as i64 > limit;
                (
                    Some(structure),
                    nodes.into_iter().take(limit as usize).collect(),
                    truncated,
                )
            } else {
                (None, Vec::new(), false)
            };

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
    /// Navigate the category tree: child categories and the books directly under
    /// a category. With no category id, returns the root categories.
    pub fn browse_categories(&self, category_id: Option<i64>, limit: i64) -> Result<CategoryView> {
        let conn = self.conn.lock().unwrap();

        let current = match category_id {
            Some(id) => conn
                .query_row("SELECT id, title FROM category WHERE id = ?1", [id], |r| {
                    Ok(NamedCategory {
                        category_id: r.get(0)?,
                        title: r.get(1)?,
                    })
                })
                .ok(),
            None => None,
        };

        const CHILD_SELECT: &str =
            "SELECT c.id, c.title, EXISTS(SELECT 1 FROM category cc WHERE cc.parentId = c.id) FROM category c ";
        let map_cat = |r: &rusqlite::Row| {
            Ok(SubCategory {
                category_id: r.get(0)?,
                title: r.get(1)?,
                has_children: r.get::<_, i64>(2)? != 0,
            })
        };
        let subcategories: Vec<SubCategory> = match category_id {
            Some(id) => {
                let mut stmt = conn.prepare(&format!(
                    "{CHILD_SELECT} WHERE c.parentId = ?1 ORDER BY c.orderIndex, c.title"
                ))?;
                let v = stmt
                    .query_map([id], map_cat)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                v
            }
            None => {
                let mut stmt = conn.prepare(&format!(
                    "{CHILD_SELECT} WHERE c.parentId IS NULL ORDER BY c.orderIndex, c.title"
                ))?;
                let v = stmt
                    .query_map([], map_cat)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                v
            }
        };

        let books = match category_id {
            Some(id) => {
                let mut stmt = conn.prepare(
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
}

// ── result types ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct BookHit {
    pub book_id: i64,
    pub title: String,
    pub he_ref: Option<String>,
    pub category: String,
    pub authors: Option<String>,
    pub total_lines: i64,
    pub is_base_book: bool,
}

#[derive(Debug, Serialize)]
pub struct TextLine {
    pub line_index: i64,
    pub he_ref: Option<String>,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct GetTextResult {
    pub book_id: i64,
    pub book_title: String,
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
}

#[derive(Debug, Serialize)]
pub struct BookInfo {
    pub book_id: i64,
    pub title: String,
    pub he_ref: Option<String>,
    pub category: String,
    pub short_desc: Option<String>,
    pub long_desc: Option<String>,
    pub source: String,
    pub total_lines: i64,
    pub is_base_book: bool,
    pub connections: Connections,
    pub has_alt_structures: bool,
    pub has_teamim: bool,
    pub has_nekudot: bool,
    pub authors: Vec<String>,
    pub generations: Vec<String>,
    pub topics: Vec<String>,
    pub acronyms: Vec<String>,
    pub versions: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct LinkRow {
    /// Connection type name, e.g. COMMENTARY, TARGUM, SOURCE, REFERENCE, MIDRASH…
    pub connection_type: String,
    /// "outgoing" (queried book is the source) or "incoming" (it is the target).
    pub direction: &'static str,
    pub source_book_id: i64,
    pub source_book_title: String,
    pub source_line_index: i64,
    pub source_ref: Option<String>,
    pub source_snippet: String,
    pub target_book_id: i64,
    pub target_book_title: String,
    pub target_line_index: i64,
    pub target_ref: Option<String>,
    pub target_snippet: String,
}

#[derive(Debug, Serialize)]
pub struct NamedBook {
    pub book_id: i64,
    pub title: String,
}

#[derive(Debug, Serialize)]
pub struct CommentaryItem {
    pub connection_type: String,
    pub commentator_book_id: i64,
    pub commentator_title: String,
    /// The base-text line this commentary is attached to.
    pub on_line_index: i64,
    pub target_line_index: i64,
    pub target_ref: Option<String>,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct Commentaries {
    pub default_commentators: Vec<NamedBook>,
    pub items: Vec<CommentaryItem>,
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
pub struct SubCategory {
    pub category_id: i64,
    pub title: String,
    pub has_children: bool,
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
    pub current: Option<NamedCategory>,
    pub subcategories: Vec<SubCategory>,
    pub books: Vec<CategoryBook>,
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// One direction of a link query (outgoing = the book is the source side).
fn query_links(
    conn: &Connection,
    book_id: i64,
    line_index: Option<i64>,
    connection_type: Option<&str>,
    outgoing: bool,
    limit: i64,
) -> Result<Vec<LinkRow>> {
    let (book_col, line_col, range_side) = if outgoing {
        ("lk.sourceBookId", "sl.lineIndex", 0)
    } else {
        ("lk.targetBookId", "tl.lineIndex", 1)
    };
    let mut sql = format!(
        "SELECT ct.name, sb.id, sb.title, sl.lineIndex, sl.heRef, sl.content, \
                tb.id, tb.title, tl.lineIndex, tl.heRef, tl.content \
           FROM link lk \
           JOIN connection_type ct ON lk.connectionTypeId = ct.id \
           JOIN book sb ON lk.sourceBookId = sb.id \
           JOIN line sl ON lk.sourceLineId = sl.id \
           JOIN book tb ON lk.targetBookId = tb.id \
           JOIN line tl ON lk.targetLineId = tl.id \
           LEFT JOIN link_range lr ON lr.linkId = lk.id AND lr.side = {range_side} \
          WHERE {book_col} = ?"
    );
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(book_id)];
    if let Some(li) = line_index {
        sql.push_str(&format!(
            " AND {line_col} <= ? AND COALESCE(lr.endLineIndex, {line_col}) >= ?"
        ));
        params.push(Box::new(li));
        params.push(Box::new(li));
    }
    if let Some(ct) = connection_type {
        sql.push_str(" AND ct.name = ?");
        params.push(Box::new(ct.to_string()));
    }
    sql.push_str(" ORDER BY lk.targetBookOrderIndex, tl.lineIndex LIMIT ?");
    params.push(Box::new(limit));

    let direction = if outgoing { "outgoing" } else { "incoming" };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(
            rusqlite::params_from_iter(params.iter().map(|b| b.as_ref())),
            |r| {
                let src_content: String = r.get(5)?;
                let tgt_content: String = r.get(10)?;
                Ok(LinkRow {
                    connection_type: r.get(0)?,
                    direction,
                    source_book_id: r.get(1)?,
                    source_book_title: r.get(2)?,
                    source_line_index: r.get(3)?,
                    source_ref: r.get(4)?,
                    source_snippet: truncate_text(&strip_html_tags(&src_content), 160),
                    target_book_id: r.get(6)?,
                    target_book_title: r.get(7)?,
                    target_line_index: r.get(8)?,
                    target_ref: r.get(9)?,
                    target_snippet: truncate_text(&strip_html_tags(&tgt_content), 160),
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Truncate to at most `n` chars (char-safe), appending `…` when cut.
fn truncate_text(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    }
}

/// Escape `%` and `_` (and the `\` escape char) for a SQL LIKE pattern.
fn escape_like(s: &str) -> String {
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

/// Strip HTML tags and decode a handful of common entities. `line.content` is
/// raw HTML (`<h1>`, `<b>`, footnote markup, …); most consumers want plain text.
pub fn strip_html_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for c in input.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&nbsp;", " ")
        .replace("&thinsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x22;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&apos;", "'")
        // decode &amp; last so we never double-decode an escaped entity
        .replace("&amp;", "&")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{strip_html_tags, Db};
    use rusqlite::Connection;
    use std::sync::{Mutex, OnceLock};

    fn fixture_db() -> Db {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE book (id INTEGER PRIMARY KEY, title TEXT NOT NULL);
             CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER NOT NULL, lineIndex INTEGER NOT NULL, heRef TEXT, content TEXT NOT NULL);
             CREATE TABLE connection_type (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
             CREATE TABLE link (id INTEGER PRIMARY KEY, sourceBookId INTEGER NOT NULL, targetBookId INTEGER NOT NULL, sourceLineId INTEGER NOT NULL, targetLineId INTEGER NOT NULL, targetLineIndex INTEGER NOT NULL, targetBookOrderIndex INTEGER NOT NULL, connectionTypeId INTEGER NOT NULL, isDeclaredBase INTEGER NOT NULL);
             CREATE TABLE link_range (linkId INTEGER NOT NULL, side INTEGER NOT NULL, endLineId INTEGER NOT NULL, endLineIndex INTEGER NOT NULL);
             CREATE TABLE default_commentator (bookId INTEGER NOT NULL, commentatorBookId INTEGER NOT NULL, position INTEGER NOT NULL);
             CREATE TABLE tocText (id INTEGER PRIMARY KEY, text TEXT NOT NULL);
             CREATE TABLE tocEntry (id INTEGER PRIMARY KEY, parentId INTEGER, level INTEGER NOT NULL, textId INTEGER NOT NULL, hasChildren INTEGER NOT NULL, lineId INTEGER, bookId INTEGER NOT NULL);
             CREATE TABLE alt_toc_structure (id INTEGER PRIMARY KEY, bookId INTEGER NOT NULL, key TEXT NOT NULL, title TEXT, heTitle TEXT);
             CREATE TABLE alt_toc_entry (id INTEGER PRIMARY KEY, structureId INTEGER NOT NULL, parentId INTEGER, textId INTEGER NOT NULL, level INTEGER NOT NULL, lineId INTEGER, isLastChild INTEGER NOT NULL, hasChildren INTEGER NOT NULL);",
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO book VALUES (1, 'בסיס'), (2, 'מפרש');
             INSERT INTO line VALUES (10, 1, 2, 'בסיס א', 'טקסט בסיס'), (11, 1, 3, 'בסיס ב', 'טקסט בסיס'), (12, 1, 4, 'בסיס ג', 'טקסט בסיס'), (20, 2, 100, 'מפרש א', 'טקסט מפרש'), (21, 2, 101, 'מפרש ב', 'טקסט מפרש');
             INSERT INTO connection_type VALUES (1, 'COMMENTARY');
             INSERT INTO link VALUES (100, 1, 2, 10, 20, 100, 1, 1, 0);
             INSERT INTO link_range VALUES (100, 0, 12, 4), (100, 1, 21, 101);
             INSERT INTO default_commentator VALUES (1, 2, 0);
             INSERT INTO tocText VALUES (1, 'פרק א'), (2, 'פרשה א');
             INSERT INTO tocEntry VALUES (1, NULL, 0, 1, 0, 10, 1);
             INSERT INTO alt_toc_structure VALUES (7, 1, 'Parasha', 'פרשה', 'פרשה');
             INSERT INTO alt_toc_entry VALUES (8, 7, NULL, 2, 0, 11, 1, 0);",
        )
        .unwrap();
        Db {
            conn: Mutex::new(conn),
            ref_index: OnceLock::new(),
        }
    }

    #[test]
    fn strips_tags_and_decodes_entities() {
        assert_eq!(strip_html_tags("<b>שלום</b>"), "שלום");
        assert_eq!(strip_html_tags("א&#x27;ב"), "א'ב");
        assert_eq!(strip_html_tags("<h1>פרק</h1> א&nbsp;ב"), "פרק א ב");
        assert_eq!(strip_html_tags("&amp;"), "&");
    }

    #[test]
    fn links_and_commentaries_include_lines_inside_a_range() {
        let db = fixture_db();

        let outgoing = db.get_links(1, Some(3), None, "outgoing", 10).unwrap();
        assert_eq!(outgoing.len(), 1);
        let incoming = db.get_links(2, Some(101), None, "incoming", 10).unwrap();
        assert_eq!(incoming.len(), 1);

        let commentaries = db.get_commentaries(1, 3, 3, None, 10).unwrap();
        assert_eq!(commentaries.items.len(), 1);
        assert_eq!(commentaries.items[0].on_line_index, 2);
    }

    #[test]
    fn toc_returns_selected_alternate_structure_entries() {
        let toc = fixture_db().get_book_toc(1, 10, Some("Parasha")).unwrap();
        assert_eq!(toc.selected_alt_structure.unwrap().key, "Parasha");
        assert_eq!(toc.alt_entries.len(), 1);
        assert_eq!(toc.alt_entries[0].line_index, Some(3));
    }
}
