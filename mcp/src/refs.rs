//! Reference resolution — find-ref ("בראשית א א", "ברכות ב.", "שו\"ע או\"ח א א",
//! "ברכות ב ע\"א", a dibbur-hamatchil …) → (book_id, line_index).
//!
//! Mirrors the app's find-ref pipeline:
//! 1. **Book**: the longest leading run of the query that names a book — an in-memory index
//!    of NORMALIZED titles, heRefs and every `book_acronym` alias (a name can map to several
//!    books; all are tried).
//! 2. **Line, canonical key** (schema 6): the rest of the query becomes the canonical
//!    reference key of `crate::refkey` (the app's `ref_key.dart`) and is looked up in the
//!    `line_ref` hash index — first the full key, then the "partial" key that omits named
//!    parts of the heRef ("טור שט ג").
//! 3. **Line, heRef prefix**: otherwise the normalized address is matched against the book's
//!    normalized `line.heRef`s (exact, else the first line it is a token-prefix of — this is
//!    what resolves a daf/chapter-level reference like "ברכות ב." to its first line).
//! 4. **TOC / alternate TOC** heading text ("בראשית נח", "פרשת לך לך").
//! 5. **Dibbur hamatchil** (`line_dh`) — the address as the opening words of a comment.
//! 6. Last resort: substring match on raw `line.heRef`.
//!
//! Normalization strips nikud/te'amim and gershayim, turns Talmud amud markers `.`→" א"
//! and `:`→" ב", and drops commas/maqaf — applied identically to the query and to names.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;

use crate::db::Caps;
use crate::refkey;

/// Normalize a reference/name for matching. Applied to book names, acronyms,
/// `line.heRef`, and the user query alike.
pub fn normalize_ref(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            // Separators → space. Maqaf (U+05BE) must be handled before the nikud
            // range below (it falls inside 0591–05C7) so words don't get glued.
            ',' | '\u{05BE}' | '-' | '\u{2013}' | '\u{2014}' | '\u{05C0}' | '|' => out.push(' '),
            '"' | '\u{05F4}' | '\u{201C}' | '\u{201D}' => {} // gershayim
            '\'' | '\u{05F3}' | '\u{2018}' | '\u{2019}' => {} // geresh
            '.' => out.push_str(" א "),                      // Talmud daf: amud alef
            ':' => out.push_str(" ב "),                      // Talmud daf: amud bet
            '(' | ')' | '[' | ']' | ';' => out.push(' '),
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

/// Find the raw address after the matched leading book name. `normalize_ref` can
/// expand punctuation into tokens (notably daf marks), so slicing the normalized
/// query loses syntax that `refkey` needs to interpret, such as a range dash or a
/// chapter:verse colon. Match at raw whitespace and separator punctuation boundaries
/// to retain the address.
fn raw_address_after_book<'a>(query: &'a str, normalized_book: &str) -> Option<&'a str> {
    for (end, c) in query.char_indices() {
        let separator_end = if c.is_whitespace()
            || matches!(
                c,
                ',' | ';' | '(' | ')' | '[' | ']' | '|' | '\u{05BE}' | '-' | '–' | '—'
            ) {
            end + c.len_utf8()
        } else {
            continue;
        };
        let prefix = query[..separator_end].trim_end();
        let expanded = refkey::expand_query_amud_marks(prefix);
        if normalize_ref(&expanded) == normalized_book {
            return Some(query[separator_end..].trim_start());
        }
    }
    None
}

pub struct RefIndex {
    /// normalized name → book ids (a name like "ברכות" can map to several books).
    name_to_books: BTreeMap<String, Vec<i64>>,
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

fn add_name(map: &mut BTreeMap<String, Vec<i64>>, name: &str, id: i64) {
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
        let mut name_to_books: BTreeMap<String, Vec<i64>> = BTreeMap::new();
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

    pub fn title(&self, id: i64) -> String {
        self.book_title.get(&id).cloned().unwrap_or_default()
    }

    pub fn is_base(&self, id: i64) -> bool {
        self.book_meta.get(&id).is_some_and(|m| m.0)
    }

    pub fn order(&self, id: i64) -> i64 {
        self.book_meta.get(&id).map_or(i64::MAX, |m| m.1)
    }

    /// Names equal to `norm` (exact) and then names starting with it (token prefix).
    pub fn names_matching(&self, norm: &str, max: usize) -> Vec<(String, Vec<i64>, bool)> {
        let mut out = Vec::new();
        if let Some(ids) = self.name_to_books.get(norm) {
            out.push((norm.to_string(), ids.clone(), true));
        }
        for (name, ids) in self.name_to_books.range(norm.to_string()..) {
            if out.len() >= max || !name.starts_with(norm) {
                break;
            }
            if name != norm {
                out.push((name.clone(), ids.clone(), false));
            }
        }
        out
    }

    fn matching_line(
        &self,
        conn: &Connection,
        book_id: i64,
        canonical: &str,
    ) -> Result<Option<CachedLine>> {
        let mut cache = self.line_cache.lock().unwrap_or_else(|p| p.into_inner());
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
    pub he_ref: Option<String>,
    pub snippet: String,
    /// How the line was found: "exact" (canonical key / heRef equal to the query),
    /// "partial" (key with named parts omitted), "prefix" (first line under the
    /// reference, e.g. a daf or chapter), "toc" (a TOC / alternate-TOC heading),
    /// "dibur_hamatchil" (opening words of a comment), or "fallback" (heRef substring).
    pub match_kind: &'static str,
}

fn kind_rank(kind: &str) -> u8 {
    match kind {
        "exact" => 0,
        "partial" => 1,
        "prefix" => 2,
        "toc" => 3,
        "dibur_hamatchil" => 4,
        _ => 5,
    }
}

/// Resolve a human reference to candidate (book_id, line_index) locations. Snippets are
/// left empty for the caller to fill (it owns the line-text decoder).
pub fn resolve(
    conn: &Connection,
    caps: &Caps,
    index: &RefIndex,
    query: &str,
    limit: i64,
) -> Result<Vec<RefCandidate>> {
    let expanded = refkey::expand_query_amud_marks(query.trim());
    let q_tokens: Vec<String> = normalize_ref(&expanded)
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
    // Base texts first, then catalogue order: the strongest candidates are tried first.
    book_ids.sort_by_key(|id| (!index.is_base(*id), index.order(*id), *id));

    let normalized_book = q_tokens[..consumed].join(" ");
    let raw_address = raw_address_after_book(query.trim(), &normalized_book)
        .map(str::to_string)
        .unwrap_or_else(|| q_tokens[consumed..].join(" "));
    let expanded_address = refkey::expand_query_amud_marks(&raw_address);
    let address = normalize_ref(&expanded_address);
    let key = refkey::build_ref_key(&raw_address);
    let mut out: Vec<RefCandidate> = Vec::new();

    for bid in book_ids.iter().copied().take(60) {
        let title = index.title(bid);
        // heRefs rarely spell locator words ("פרק", "עמוד" from ע"א …): retry the
        // heRef match with the canonical key's tokens when they differ.
        let key_address = key.clone().filter(|k| *k != address);
        let found = if let Some(c) = by_line_ref(conn, caps, bid, key.as_deref())? {
            Some(c)
        } else if let Some(c) = by_heref(conn, index, bid, &address)? {
            Some(c)
        } else if let Some(c) = match &key_address {
            Some(k) => by_heref(conn, index, bid, k)?,
            None => None,
        } {
            Some(c)
        } else if !address.is_empty() {
            match by_toc(conn, caps, bid, &raw_address)? {
                Some(c) => Some(c),
                None => by_dibur(conn, caps, bid, &address)?,
            }
        } else {
            None
        };
        if let Some((line_index, he_ref, kind)) = found {
            out.push(RefCandidate {
                book_id: bid,
                book_title: title,
                line_index,
                he_ref,
                snippet: String::new(),
                match_kind: kind,
            });
        }
    }

    if out.is_empty() {
        return fallback_heref_like(conn, query, limit);
    }

    // Rank: best match kind first; base texts before commentaries; then catalogue order.
    out.sort_by_key(|c| {
        (
            kind_rank(c.match_kind),
            !index.is_base(c.book_id),
            index.order(c.book_id),
            c.book_id,
        )
    });
    out.truncate(limit.max(1) as usize);
    Ok(out)
}

type Found = Option<(i64, Option<String>, &'static str)>;

/// Canonical key → `line_ref` (full key, then the partial key).
fn by_line_ref(conn: &Connection, caps: &Caps, book_id: i64, key: Option<&str>) -> Result<Found> {
    let Some(key) = key else { return Ok(None) };
    if !caps.line_ref {
        return Ok(None);
    }
    for (k, kind) in [
        (key.to_string(), "exact"),
        (
            format!("{} {key}", refkey::PARTIAL_REF_KEY_MARKER),
            "partial",
        ),
    ] {
        let row: Option<(i64, Option<String>)> = conn
            .query_row(
                "SELECT lr.lineIndex, l.heRef FROM line_ref lr \
                   JOIN line l ON l.bookId = lr.bookId AND l.lineIndex = lr.lineIndex \
                  WHERE lr.bookId = ?1 AND lr.refKeyHash = ?2 ORDER BY lr.lineIndex LIMIT 1",
                rusqlite::params![book_id, refkey::ref_key_hash(&k)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((li, he_ref)) = row {
            return Ok(Some((li, he_ref, kind)));
        }
    }
    Ok(None)
}

/// Normalized heRef: exact, else the first line the address is a token-prefix of.
fn by_heref(conn: &Connection, index: &RefIndex, book_id: i64, address: &str) -> Result<Found> {
    let book_href = index.book_heref.get(&book_id).cloned().unwrap_or_default();
    let mut canonical = normalize_ref(&book_href);
    if !address.is_empty() {
        canonical.push(' ');
        canonical.push_str(address);
    }
    let Some(line) = index.matching_line(conn, book_id, &canonical)? else {
        return Ok(None);
    };
    let kind = if normalize_ref(&line.he_ref) == canonical {
        "exact"
    } else {
        "prefix"
    };
    Ok(Some((line.line_index, Some(line.he_ref), kind)))
}

/// A TOC or alternate-TOC heading whose normalized text equals the address (locator
/// words such as "פרק"/"סימן" ignored on both sides, as in the canonical key).
fn by_toc(conn: &Connection, caps: &Caps, book_id: i64, address: &str) -> Result<Found> {
    let Some(want) = refkey::build_ref_key(address) else {
        return Ok(None);
    };
    let strip_parasha = |k: &str| k.strip_prefix("פרשת ").unwrap_or(k).to_string();
    let want = strip_parasha(&want);
    let mut sql = String::from(
        "SELECT tt.text, l.lineIndex, l.heRef FROM tocEntry te JOIN tocText tt ON tt.id = te.textId \
           JOIN line l ON l.id = te.lineId WHERE te.bookId = ?1",
    );
    if caps.alt_toc {
        sql.push_str(
            " UNION ALL SELECT tt.text, l.lineIndex, l.heRef FROM alt_toc_entry ae \
                JOIN alt_toc_structure s ON s.id = ae.structureId \
                JOIN tocText tt ON tt.id = ae.textId JOIN line l ON l.id = ae.lineId \
               WHERE s.bookId = ?1",
        );
    }
    let mut stmt = conn.prepare(&sql)?;
    let mut best: Found = None;
    for row in stmt.query_map([book_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })? {
        let (text, li, he_ref) = row?;
        let Some(k) = refkey::build_ref_key(&crate::text::strip_html_tags(&text)) else {
            continue;
        };
        if strip_parasha(&k) == want && best.as_ref().is_none_or(|b| li < b.0) {
            best = Some((li, he_ref, "toc"));
        }
    }
    Ok(best)
}

/// The address as the opening words of a comment (`line_dh.dhText`, normalized without
/// gershayim/punctuation).
fn by_dibur(conn: &Connection, caps: &Caps, book_id: i64, address: &str) -> Result<Found> {
    if !caps.line_dh || address.chars().count() < 2 {
        return Ok(None);
    }
    let row: Option<(i64, Option<String>)> = conn
        .query_row(
            "SELECT d.lineIndex, l.heRef FROM line_dh d \
               JOIN line l ON l.bookId = d.bookId AND l.lineIndex = d.lineIndex \
              WHERE d.bookId = ?1 AND d.dhText >= ?2 AND d.dhText < ?2 || char(1114111) \
              ORDER BY d.lineIndex LIMIT 1",
            rusqlite::params![book_id, address],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row.map(|(li, he_ref)| (li, he_ref, "dibur_hamatchil")))
}

/// Last resort: substring match on raw `line.heRef`.
fn fallback_heref_like(conn: &Connection, query: &str, limit: i64) -> Result<Vec<RefCandidate>> {
    let q = query.trim();
    if q.chars().count() < 2 {
        return Ok(vec![]);
    }
    let like = format!("%{}%", crate::db::escape_like(q));
    let mut stmt = conn.prepare(
        "SELECT l.bookId, b.title, l.lineIndex, l.heRef \
           FROM line l JOIN book b ON l.bookId = b.id \
          WHERE l.heRef LIKE ?1 ESCAPE '\\' \
          ORDER BY b.orderIndex, l.lineIndex LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![like, limit], |r| {
        Ok(RefCandidate {
            book_id: r.get(0)?,
            book_title: r.get(1)?,
            line_index: r.get(2)?,
            he_ref: r.get(3)?,
            snippet: String::new(),
            match_kind: "fallback",
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::{normalize_ref, starts_with_tokens, RefIndex};
    use crate::db::Caps;
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

    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE book (id INTEGER PRIMARY KEY, title TEXT NOT NULL, heRef TEXT, isBaseBook INTEGER NOT NULL, orderIndex INTEGER NOT NULL);
             CREATE TABLE book_acronym (bookId INTEGER NOT NULL, term TEXT NOT NULL);
             CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER NOT NULL, lineIndex INTEGER NOT NULL, heRef TEXT);
             CREATE TABLE line_ref (bookId INTEGER NOT NULL, refKeyHash INTEGER NOT NULL, lineIndex INTEGER NOT NULL);
             CREATE TABLE line_dh (bookId INTEGER NOT NULL, dhText TEXT NOT NULL, lineIndex INTEGER NOT NULL, dhDisplay TEXT NOT NULL);
             CREATE TABLE tocText (id INTEGER PRIMARY KEY, text TEXT NOT NULL);
             CREATE TABLE tocEntry (id INTEGER PRIMARY KEY, bookId INTEGER NOT NULL, parentId INTEGER, textId INTEGER NOT NULL, level INTEGER NOT NULL, lineId INTEGER, hasChildren INTEGER NOT NULL DEFAULT 0);
             INSERT INTO book VALUES (1, 'בראשית', 'בראשית', 1, 1), (103, 'ברכות', 'ברכות', 1, 1), (9, 'רש\"י על ברכות', 'רש\"י על ברכות', 0, 5), (28, 'תהילים', 'תהילים', 1, 2);
             INSERT INTO book_acronym VALUES (1, 'ספר בראשית'), (1, 'ספר־בראשית'), (28, 'תהלים');
             INSERT INTO line VALUES (1, 1, 2, 'בראשית א, א'), (2, 103, 2, 'ברכות, ב., א'), (3, 103, 17, 'ברכות, ב:, א'), (4, 9, 5, 'רש\"י על ברכות, ב., א'), (5, 28, 1160, 'תהילים, עא, א'), (6, 28, 1185, 'תהילים, עב, א');
             INSERT INTO tocText VALUES (1, 'פרק ראשון');
             INSERT INTO tocEntry VALUES (1, 103, NULL, 1, 1, 2, 0);
             INSERT INTO line_dh VALUES (9, 'מאימתי קורין', 5, 'מאימתי קורין');",
        )
        .unwrap();
        // line_ref rows exactly as the library builder writes them (FNV of the key).
        conn.execute(
            "INSERT INTO line_ref VALUES (103, ?1, 2)",
            [crate::refkey::ref_key_hash("ב א א")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO line_ref VALUES (1, ?1, 2)",
            [crate::refkey::ref_key_hash("א א")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO line_ref VALUES (103, ?1, 2)",
            [crate::refkey::ref_key_hash("~ ב א")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO line_ref VALUES (28, ?1, 1160), (28, ?2, 1185), (28, ?3, 1160)",
            rusqlite::params![
                crate::refkey::ref_key_hash("עא א"),
                crate::refkey::ref_key_hash("עב א"),
                crate::refkey::ref_key_hash("~ עא"),
            ],
        )
        .unwrap();
        conn
    }

    fn caps() -> Caps {
        Caps {
            line_ref: true,
            line_dh: true,
            ..Caps::default()
        }
    }

    #[test]
    fn resolves_via_line_ref_heref_prefix_toc_and_dibur() {
        let conn = fixture();
        let index = RefIndex::build(&conn);

        let c = super::resolve(&conn, &caps(), &index, "ברכות ב., א", 3).unwrap();
        assert_eq!(
            (c[0].book_id, c[0].line_index, c[0].match_kind),
            (103, 2, "exact")
        );

        // amud-level reference → the first line under it (heRef prefix)
        let c = super::resolve(&conn, &caps(), &index, "ברכות ב:", 3).unwrap();
        assert_eq!(
            (c[0].book_id, c[0].line_index, c[0].match_kind),
            (103, 17, "prefix")
        );

        // acronym/alias book name
        let c = super::resolve(&conn, &caps(), &index, "ספר בראשית א א", 3).unwrap();
        assert_eq!((c[0].book_id, c[0].line_index), (1, 2));

        // TOC heading text
        let c = super::resolve(&conn, &caps(), &index, "ברכות פרק ראשון", 3).unwrap();
        assert_eq!((c[0].line_index, c[0].match_kind), (2, "toc"));

        // dibbur hamatchil
        let c = super::resolve(&conn, &caps(), &index, "רש\"י על ברכות מאימתי", 3).unwrap();
        assert_eq!(
            (c[0].book_id, c[0].line_index, c[0].match_kind),
            (9, 5, "dibur_hamatchil")
        );
    }

    #[test]
    fn pipeline_preserves_raw_ranges_and_chapter_verse_colons() {
        let conn = fixture();
        let index = RefIndex::build(&conn);

        for query in [
            "בראשית א א-ג",
            "בְּרֵאשִׁית א:א",
            "בראשית,א,א-ג",
            "בראשית,א:א",
            "סֵפֶר־בְּרֵאשִׁית,א א-ג",
        ] {
            let found = super::resolve(&conn, &caps(), &index, query, 1).unwrap();
            assert_eq!(
                (found[0].book_id, found[0].line_index, found[0].match_kind),
                (1, 2, "exact"),
                "query: {query}"
            );
        }

        for (query, line_index) in [
            ("תהילים ע\"א א", 1160),
            ("תהלים ע\"א א", 1160),
            ("תהילים ע\"ב א", 1185),
            ("תהילים פרק ע\"א פסוק א", 1160),
            ("תהילים ע\"א-ע\"ב", 1160),
        ] {
            let found = super::resolve(&conn, &caps(), &index, query, 1).unwrap();
            assert_eq!(
                (found[0].book_id, found[0].line_index),
                (28, line_index),
                "query: {query}"
            );
        }

        for query in ["ברכות ב.-ג.", "ברכות,ב.-ג."] {
            let found = super::resolve(&conn, &caps(), &index, query, 1).unwrap();
            assert_eq!(
                (found[0].book_id, found[0].line_index, found[0].match_kind),
                (103, 2, "partial"),
                "query: {query}"
            );
        }

        // The original raw form still canonicalizes ע"א/ע"ב after daf expansion.
        conn.execute(
            "INSERT INTO line_ref VALUES (103, ?1, 17)",
            [crate::refkey::ref_key_hash("~ ב ב")],
        )
        .unwrap();
        for (query, line_index) in [("ברכות ב ע\"א", 2), ("ברכות ב ע\"ב", 17)] {
            let found = super::resolve(&conn, &caps(), &index, query, 1).unwrap();
            assert_eq!(
                (found[0].line_index, found[0].match_kind),
                (line_index, "partial")
            );
        }
    }

    #[test]
    fn resolves_from_the_per_book_normalized_cache() {
        let conn = fixture();
        let index = RefIndex::build(&conn);
        let first = super::resolve(&conn, &Caps::default(), &index, "בראשית א א", 1).unwrap();
        let second = super::resolve(&conn, &Caps::default(), &index, "בראשית א א", 1).unwrap();
        assert_eq!(first[0].line_index, 2);
        assert_eq!(second[0].line_index, 2);
        assert_eq!(index.line_cache.lock().unwrap().len(), 1);
    }

    #[test]
    fn names_matching_lists_exact_then_prefix() {
        let conn = fixture();
        let index = RefIndex::build(&conn);
        let m = index.names_matching(&normalize_ref("ברכות"), 10);
        assert!(m[0].2, "exact first");
        assert_eq!(m[0].1, vec![103]);
        assert!(
            m.iter()
                .any(|(n, _, exact)| !exact && n.starts_with("ברכות"))
                || m.len() == 1
        );
    }
}
