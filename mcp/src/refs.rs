//! Reference resolution ("בראשית א א", "שו\"ע או\"ח א א", "ברכות ב:") → (book_id, line_index).
//!
//! Mirrors the app's find_ref approach: an in-memory index of NORMALIZED book
//! names (title + heRef + every book_acronym alias) → book ids, then the trailing
//! address tokens are matched against normalized `line.heRef` within each candidate
//! book. Normalization strips nikud/te'amim and gershayim, turns Talmud amud markers
//! `.`→" א" and `:`→" ב", and drops commas/maqaf — applied identically to query and
//! heRef so they line up. This is the biggest correctness-risk tool; it is covered by
//! a golden test corpus (see tests/).

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

/// Normalize a reference/name for matching. Applied to book names, acronyms,
/// `line.heRef`, and the user query alike.
pub fn normalize_ref(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            // Separators → space. Maqaf (U+05BE) must be handled before the nikud
            // range below (it falls inside 0591–05C7) so words don't get glued.
            ',' | '־' | '-' | '\u{2013}' | '\u{2014}' => out.push(' '),
            '"' | '\u{05F4}' => {}      // gershayim (ASCII + HEBREW GERSHAYIM)
            '\'' | '\u{05F3}' => {}     // geresh
            '.' => out.push_str(" א "), // Talmud daf: amud alef
            ':' => out.push_str(" ב "), // Talmud daf: amud bet
            '\u{0591}'..='\u{05C7}' => {} // Hebrew nikud + te'amim
            _ => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Token-boundary prefix test on normalized (space-separated) strings.
fn starts_with_tokens(hay: &str, needle: &str) -> bool {
    hay == needle
        || (!needle.is_empty() && hay.starts_with(needle) && hay[needle.len()..].starts_with(' '))
}

pub struct RefIndex {
    /// normalized name → book ids (a name like "ברכות" can map to several books).
    name_to_books: HashMap<String, Vec<i64>>,
    book_heref: HashMap<i64, String>,
    book_title: HashMap<i64, String>,
    /// bookId → (is_base_book, orderIndex) for ranking candidates.
    book_meta: HashMap<i64, (bool, i64)>,
    /// Lazily built normalized heRef index for each requested book.
    line_cache: Mutex<HashMap<i64, BTreeMap<String, CachedLine>>>,
}

#[derive(Clone)]
struct CachedLine {
    line_index: i64,
    he_ref: String,
}

fn add_name(map: &mut HashMap<String, Vec<i64>>, name: &str, id: i64) {
    let n = normalize_ref(name);
    if n.is_empty() {
        return;
    }
    let v = map.entry(n).or_default();
    if !v.contains(&id) {
        v.push(id);
    }
}

impl RefIndex {
    pub fn build(conn: &Connection) -> RefIndex {
        let mut name_to_books: HashMap<String, Vec<i64>> = HashMap::new();
        let mut book_heref = HashMap::new();
        let mut book_title = HashMap::new();
        let mut book_meta = HashMap::new();

        if let Ok(mut stmt) =
            conn.prepare("SELECT id, title, heRef, isBaseBook, orderIndex FROM book")
        {
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            });
            if let Ok(rows) = rows {
                for (id, title, heref, is_base, order) in rows.flatten() {
                    let href = heref.clone().unwrap_or_else(|| title.clone());
                    add_name(&mut name_to_books, &title, id);
                    if let Some(h) = &heref {
                        add_name(&mut name_to_books, h, id);
                    }
                    book_title.insert(id, title);
                    book_heref.insert(id, href);
                    book_meta.insert(id, (is_base != 0, order));
                }
            }
        }

        if let Ok(mut stmt) = conn.prepare("SELECT bookId, term FROM book_acronym") {
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)));
            if let Ok(rows) = rows {
                for (id, term) in rows.flatten() {
                    add_name(&mut name_to_books, &term, id);
                }
            }
        }

        RefIndex {
            name_to_books,
            book_heref,
            book_title,
            book_meta,
            line_cache: Mutex::new(HashMap::new()),
        }
    }

    fn matching_line(
        &self,
        conn: &Connection,
        book_id: i64,
        canonical: &str,
    ) -> Result<Option<CachedLine>> {
        let mut cache = self.line_cache.lock().unwrap();
        if let Entry::Vacant(entry) = cache.entry(book_id) {
            let mut stmt = conn.prepare(
                "SELECT lineIndex, heRef FROM line \
                  WHERE bookId = ?1 AND heRef IS NOT NULL ORDER BY lineIndex",
            )?;
            let mut lines = BTreeMap::new();
            for row in stmt.query_map([book_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })? {
                let (line_index, he_ref) = row?;
                let normalized = normalize_ref(&he_ref);
                lines
                    .entry(normalized)
                    .or_insert(CachedLine { line_index, he_ref });
            }
            entry.insert(lines);
        }

        let Some(lines) = cache.get(&book_id) else {
            return Ok(None);
        };
        if let Some(line) = lines.get(canonical) {
            return Ok(Some(line.clone()));
        }

        Ok(lines
            .range(canonical.to_string()..)
            .take_while(|(he_ref, _)| starts_with_tokens(he_ref, canonical))
            .map(|(_, line)| line)
            .min_by_key(|line| line.line_index)
            .cloned())
    }
}

#[derive(Debug, Serialize)]
pub struct RefCandidate {
    pub book_id: i64,
    pub book_title: String,
    pub line_index: i64,
    pub he_ref: String,
    pub snippet: String,
    /// "exact" (normalized heRef == query) or "prefix" (query is a leading portion).
    pub match_kind: &'static str,
}

/// Resolve a human reference to candidate (book_id, line_index) locations.
pub fn resolve(
    conn: &Connection,
    index: &RefIndex,
    query: &str,
    limit: i64,
) -> Result<Vec<RefCandidate>> {
    let q_tokens: Vec<String> = normalize_ref(query)
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if q_tokens.is_empty() {
        return Ok(vec![]);
    }

    // Longest leading token-run that names a book.
    let max_k = q_tokens.len().min(12);
    let mut book_ids: Vec<i64> = Vec::new();
    let mut consumed = 0usize;
    for k in (1..=max_k).rev() {
        let key = q_tokens[..k].join(" ");
        if let Some(ids) = index.name_to_books.get(&key) {
            book_ids = ids.clone();
            consumed = k;
            break;
        }
    }

    if book_ids.is_empty() {
        return fallback_heref_like(conn, query, limit);
    }

    let address = q_tokens[consumed..].join(" ");
    let mut out: Vec<RefCandidate> = Vec::new();

    for bid in book_ids {
        if out.len() as i64 >= limit {
            break;
        }
        let title = index.book_title.get(&bid).cloned().unwrap_or_default();
        let book_href = index.book_heref.get(&bid).cloned().unwrap_or_default();
        let mut canonical = normalize_ref(&book_href);
        if !address.is_empty() {
            canonical.push(' ');
            canonical.push_str(&address);
        }

        if let Some(c) = first_line_matching(conn, index, bid, &title, &canonical)? {
            out.push(c);
        }
    }

    if out.is_empty() {
        return fallback_heref_like(conn, query, limit);
    }

    // Rank: exact before prefix; base texts before commentaries; then catalogue order.
    out.sort_by_key(|c| {
        let (is_base, order) = index
            .book_meta
            .get(&c.book_id)
            .copied()
            .unwrap_or((false, i64::MAX));
        let mk = match c.match_kind {
            "exact" => 0,
            "prefix" => 1,
            _ => 2,
        };
        (mk, !is_base, order, c.book_id)
    });
    Ok(out)
}

/// Find the first line of `book_id` whose normalized heRef equals (exact) or is
/// prefixed by (prefix) `canonical`. Prefers an exact match if one exists.
fn first_line_matching(
    conn: &Connection,
    index: &RefIndex,
    book_id: i64,
    title: &str,
    canonical: &str,
) -> Result<Option<RefCandidate>> {
    let Some(line) = index.matching_line(conn, book_id, canonical)? else {
        return Ok(None);
    };
    let content = conn.query_row(
        "SELECT content FROM line WHERE bookId = ?1 AND lineIndex = ?2",
        rusqlite::params![book_id, line.line_index],
        |r| r.get::<_, String>(0),
    )?;
    let match_kind = if normalize_ref(&line.he_ref) == canonical {
        "exact"
    } else {
        "prefix"
    };
    Ok(Some(make_candidate(
        book_id,
        title,
        line.line_index,
        line.he_ref,
        content,
        match_kind,
    )))
}

/// Last resort: substring match on raw `line.heRef`.
fn fallback_heref_like(conn: &Connection, query: &str, limit: i64) -> Result<Vec<RefCandidate>> {
    let like = format!("%{}%", query.replace(['%', '_'], ""));
    let mut stmt = conn.prepare(
        "SELECT l.bookId, b.title, l.lineIndex, l.heRef, l.content \
           FROM line l JOIN book b ON l.bookId = b.id \
          WHERE l.heRef LIKE ?1 \
          ORDER BY b.orderIndex, l.lineIndex LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![like, limit], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    Ok(rows
        .flatten()
        .map(|(bid, title, li, href, content)| {
            make_candidate(bid, &title, li, href, content, "fallback")
        })
        .collect())
}

fn make_candidate(
    book_id: i64,
    title: &str,
    line_index: i64,
    he_ref: String,
    content: String,
    match_kind: &'static str,
) -> RefCandidate {
    let plain = crate::db::strip_html_tags(&content);
    let snippet: String = plain.chars().take(160).collect();
    RefCandidate {
        book_id,
        book_title: title.to_string(),
        line_index,
        he_ref,
        snippet,
        match_kind,
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_ref, starts_with_tokens, RefIndex};
    use rusqlite::Connection;

    #[test]
    fn strips_nikud_and_gershayim() {
        assert_eq!(normalize_ref("בְּרֵאשִׁית"), "בראשית");
        assert_eq!(normalize_ref("שו\"ע או\"ח"), "שוע אוח");
        assert_eq!(normalize_ref("רד\"ק"), "רדק");
    }

    #[test]
    fn expands_talmud_amud_markers() {
        assert_eq!(normalize_ref("ברכות ב."), "ברכות ב א");
        assert_eq!(normalize_ref("ברכות ב:"), "ברכות ב ב");
    }

    #[test]
    fn maqaf_and_commas_become_spaces() {
        assert_eq!(normalize_ref("על־פני"), "על פני");
        assert_eq!(normalize_ref("בראשית א, א"), "בראשית א א");
    }

    #[test]
    fn token_prefix_respects_boundaries() {
        assert!(starts_with_tokens("בראשית א א", "בראשית א"));
        assert!(starts_with_tokens("בראשית", "בראשית"));
        assert!(!starts_with_tokens("בראשית אב", "בראשית א"));
    }

    #[test]
    fn resolves_from_the_per_book_normalized_cache() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE book (id INTEGER PRIMARY KEY, title TEXT NOT NULL, heRef TEXT, isBaseBook INTEGER NOT NULL, orderIndex INTEGER NOT NULL);
             CREATE TABLE book_acronym (bookId INTEGER NOT NULL, term TEXT NOT NULL);
             CREATE TABLE line (bookId INTEGER NOT NULL, lineIndex INTEGER NOT NULL, heRef TEXT, content TEXT NOT NULL);
             INSERT INTO book VALUES (1, 'בראשית', 'בראשית', 1, 1);
             INSERT INTO line VALUES (1, 2, 'בראשית א, א', 'בתחילה');",
        )
        .unwrap();
        let index = RefIndex::build(&conn);

        let first = super::resolve(&conn, &index, "בראשית א א", 1).unwrap();
        let second = super::resolve(&conn, &index, "בראשית א א", 1).unwrap();

        assert_eq!(first[0].line_index, 2);
        assert_eq!(second[0].line_index, 2);
        assert_eq!(index.line_cache.lock().unwrap().len(), 1);
    }
}
