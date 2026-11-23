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
//! 4. **TOC / alternate TOC** heading text ("בראשית נח", "פרשת לך לך"), then the app's
//!    token-by-token tree search (`crate::toc_search`). Every fitting heading is returned:
//!    a multi-part book (ב"ח, טור) yields one candidate per part unless a part is named —
//!    in the address ("ב"ח יו"ד קא") or by the alias itself ("טור יו"ד") — which then
//!    restricts every step to that part.
//! 5. **Dibbur hamatchil** (`line_dh`) — the address as the opening words of a comment.
//! 6. Last resort: substring match on raw `line.heRef`.
//!
//! Citation forms: when steps 2-3 find nothing for the address
//! as typed, they are retried with a cleaned address: unit markers dropped (`סי'`, `סע'`,
//! `ס"ק`), marker-prefixed numbers reduced (`פ"ב ה"א` → `ב א`, `סק"ב` → `ב`), a Shulchan
//! Aruch part name dropped after a nosei kelim (`מג"א או"ח שח ב`), and leading words of the
//! book's own title dropped (`קצות החושן כח א`, where only `קצות` is an alias). A trailing
//! `ד"ה <words>` then picks that dibbur's line inside the location found ("dibur_in_ref").
//!
//! Normalization strips nikud/te'amim and gershayim, turns Talmud amud markers `.`→" א"
//! and `:`→" ב", and drops commas/maqaf — applied identically to the query and to names.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;

use crate::db::Caps;
use crate::refkey;
use crate::toc_search::{BookToc, TocCache, TocHeading};

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
    line_cache: Mutex<HashMap<i64, Arc<BookLines>>>,
    /// Lazily loaded TOC trees (the app's hierarchical TOC search).
    toc: TocCache,
}

struct CachedLine {
    line_index: i64,
    he_ref: String,
    other_indexes: Vec<i64>,
}

type BookLines = BTreeMap<String, CachedLine>;

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
            toc: TocCache::default(),
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

    /// Line indexes of every line whose normalized heRef equals or token-extends `canonical`.
    fn lines_under(
        &self,
        conn: &Connection,
        book_id: i64,
        canonical: &str,
    ) -> Result<HashSet<i64>> {
        let lines = self.book_lines(conn, book_id)?;
        Ok(lines
            .range(canonical.to_string()..)
            .take_while(|(he_ref, _)| he_ref.starts_with(canonical))
            .filter(|(he_ref, _)| starts_with_tokens(he_ref, canonical))
            .flat_map(|(_, line)| {
                std::iter::once(line.line_index).chain(line.other_indexes.iter().copied())
            })
            .collect())
    }

    fn book_lines(&self, conn: &Connection, book_id: i64) -> Result<Arc<BookLines>> {
        let mut cache = self.line_cache.lock().unwrap_or_else(|p| p.into_inner());
        let entry = match cache.entry(book_id) {
            Entry::Occupied(entry) => return Ok(entry.get().clone()),
            Entry::Vacant(entry) => entry,
        };
        let mut stmt = conn.prepare(
            "SELECT lineIndex, heRef FROM line \
              WHERE bookId = ?1 AND heRef IS NOT NULL ORDER BY lineIndex",
        )?;
        let mut lines = BookLines::new();
        for row in stmt.query_map([book_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })? {
            let (line_index, he_ref) = row?;
            match lines.entry(normalize_ref(&he_ref)) {
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    entry.get_mut().other_indexes.push(line_index);
                }
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(CachedLine {
                        line_index,
                        he_ref,
                        other_indexes: Vec::new(),
                    });
                }
            }
        }
        Ok(entry.insert(Arc::new(lines)).clone())
    }

    fn matching_line(
        &self,
        conn: &Connection,
        book_id: i64,
        canonical: &str,
    ) -> Result<Option<(i64, String)>> {
        let lines = self.book_lines(conn, book_id)?;
        if let Some(line) = lines.get(canonical) {
            return Ok(Some((line.line_index, line.he_ref.clone())));
        }

        Ok(lines
            .range(canonical.to_string()..)
            .take_while(|(he_ref, _)| starts_with_tokens(he_ref, canonical))
            .map(|(_, line)| line)
            .min_by_key(|line| line.line_index)
            .map(|line| (line.line_index, line.he_ref.clone())))
    }
}

#[derive(Debug, Serialize)]
pub struct RefCandidate {
    pub book_id: i64,
    pub book_title: String,
    pub line_index: i64,
    pub he_ref: Option<String>,
    /// For a TOC match: the heading's path ("אורח חיים > סימן קא") — heading lines often
    /// have no heRef, and in a multi-part book the path tells the parts apart.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toc_path: Option<String>,
    pub snippet: String,
    /// How the line was found: "exact" (canonical key / heRef equal to the query),
    /// "partial" (key with named parts omitted), "prefix" (first line under the
    /// reference, e.g. a daf or chapter), "dibur_in_ref" (the `ד"ה` line inside the
    /// location), "toc" (a TOC / alternate-TOC heading), "toc_partial" (the deepest
    /// heading the rest of the address could not descend below),
    /// "dibur_hamatchil" (opening words of a comment), or "fallback" (heRef substring).
    /// A reference that exists in several parts of one book ("ב\"ח קא": אורח חיים, יורה
    /// דעה, אבן העזר, חושן משפט) yields one candidate per part.
    pub match_kind: &'static str,
}

fn kind_rank(kind: &str) -> u8 {
    match kind {
        "exact" | "dibur_in_ref" => 0,
        "partial" => 1,
        "prefix" => 2,
        "toc" => 3,
        "toc_partial" => 4,
        "dibur_hamatchil" => 5,
        _ => 6,
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
    let (ref_part, dibbur) = split_dibbur(query.trim());
    let expanded = refkey::expand_query_amud_marks(&ref_part);
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
        return fallback_heref_like(conn, &ref_part, limit);
    }
    // Base texts first, then catalogue order: the strongest candidates are tried first.
    book_ids.sort_by_key(|id| (!index.is_base(*id), index.order(*id), *id));

    let normalized_book = q_tokens[..consumed].join(" ");
    // The address as typed (range dashes, chapter:verse colons kept for refkey), with
    // the dibbur hamatchil already split off.
    let raw_address = raw_address_after_book(&ref_part, &normalized_book)
        .map(str::to_string)
        .unwrap_or_else(|| q_tokens[consumed..].join(" "));
    let expanded_address = refkey::expand_query_amud_marks(&raw_address);
    let address = normalize_ref(&expanded_address);
    let raw_tokens: Vec<&str> = expanded_address.split_whitespace().collect();
    let toc_address = normalize_ref(&clean_address(&raw_tokens).join(" "));
    // The address with citation markers cleaned up, tried only when the typed one fails.
    let cleaned = drop_leading(&toc_address, |t| PART_NAMES.contains(&t));
    // The address for the TOC search: unit markers dropped, part names KEPT ("יו"ד קא").
    let toc_tokens: Vec<String> = toc_address.split_whitespace().map(str::to_string).collect();
    let name_tokens: Vec<String> = q_tokens[..consumed].to_vec();
    let mut out: Vec<RefCandidate> = Vec::new();

    for bid in book_ids.iter().copied().take(60) {
        let title = index.title(bid);
        let book_href = index.book_heref.get(&bid).cloned().unwrap_or_default();
        let own_words: Vec<String> = normalize_ref(&format!("{title} {book_href}"))
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let without_title = drop_leading(&cleaned, |t| own_words.iter().any(|w| w == t));

        // A part of the book named by the alias itself ("טור יו"ד" → "יו"ד"), or by the
        // first word of the address ("ב"ח יו"ד קא"), spelled out as the heRefs spell it.
        let section = acronym_section_tokens(&name_tokens, &own_words);
        let may_have_part = toc_tokens
            .first()
            .is_some_and(|t| may_name_a_part(t) || PART_NAMES.contains(&t.as_str()));
        let mut toc: Option<Arc<BookToc>> = if !section.is_empty() || may_have_part {
            index.toc.get(conn, bid, caps.alt_toc).ok()
        } else {
            None
        };
        let mut part_words: Option<Vec<String>> = None;
        let mut spelled: Vec<String> = Vec::new();
        if let Some(toc) = &toc {
            if let Some((sp, _)) = toc
                .named_part(&section)
                .filter(|(_, len)| *len == section.len())
            {
                spelled.push(join_nonempty(&sp.join(" "), &cleaned));
                part_words = Some(sp);
            } else if let Some((sp, len)) = toc.named_part(&toc_tokens) {
                spelled.push(join_nonempty(&sp.join(" "), &toc_tokens[len..].join(" ")));
                part_words = Some(sp);
            }
        }

        // (address to match heRefs with, text the canonical key is built from): the typed
        // address keys off its raw form, so ranges and contextual ע"א/ע"ב survive. A
        // spelled-out part comes first: the key with the part beats the one without it.
        let mut variants: Vec<(&str, &str)> = Vec::new();
        for v in &spelled {
            variants.push((v.as_str(), v.as_str()));
        }
        if !variants.iter().any(|(a, _)| *a == address) {
            variants.push((address.as_str(), raw_address.as_str()));
        }
        for v in [cleaned.as_str(), without_title.as_str()] {
            // A cleanup that leaves nothing would just point at the book's start.
            if !variants.iter().any(|(a, _)| *a == v) && (!v.is_empty() || address.is_empty()) {
                variants.push((v, v));
            }
        }

        let mut found = Vec::new();
        let mut used = address.clone();
        for (vi, (v, key_source)) in variants.iter().enumerate() {
            let key = refkey::build_ref_key(key_source);
            if let Some(h) = by_line_ref(conn, caps, bid, key.as_deref())? {
                // a part was named: a hit in another part is not this reference
                if part_words.as_ref().is_none_or(|p| in_part(&h.he_ref, p)) {
                    found.push(h);
                    used = key.unwrap_or_else(|| v.to_string());
                    break;
                }
            }
            // The key with the part's name omitted ("טור שט ג"): one candidate per part
            // (a single number names a heading, not a line: "ב"ח קא" goes to the TOC).
            if let Some(k) = key
                .as_deref()
                .filter(|k| vi >= spelled.len() && refkey::ref_key_tokens(k).len() >= 2)
            {
                let mut hits = by_partial_line_ref(conn, caps, bid, k)?;
                if let Some(p) = &part_words {
                    hits.retain(|h| in_part(&h.he_ref, p));
                }
                if !hits.is_empty() {
                    found = hits;
                    used = k.to_string();
                    break;
                }
            }
            // heRefs rarely spell locator words ("פרק", "עמוד" from ע"א …): retry the
            // heRef match with the canonical key's tokens when they differ.
            let key_address = key.as_deref().filter(|k| k != v);
            for scope in std::iter::once(*v).chain(key_address) {
                if let Some(h) = by_heref(conn, index, bid, scope)? {
                    if part_words.as_ref().is_some_and(|p| !in_part(&h.he_ref, p)) {
                        continue;
                    }
                    found.push(h);
                    used = scope.to_string();
                    break;
                }
            }
            if !found.is_empty() {
                break;
            }
        }
        if found.is_empty() && !address.is_empty() {
            let lookup = match &toc {
                Some(lookup) => lookup,
                None => toc.insert(index.toc.get(conn, bid, caps.alt_toc)?),
            };
            let (hits, complete) = by_toc_tree(
                lookup,
                &section,
                part_words.as_deref(),
                &toc_tokens,
                &raw_address,
            );
            // a heading reached by part of the address only yields to a dibbur hamatchil
            if complete && !hits.is_empty() {
                found = hits;
            } else if let Some(c) = by_dibur(conn, caps, bid, &address)? {
                found.push(c);
            } else {
                found = hits;
            }
        }
        if let Some(dh) = &dibbur {
            for f in found.iter_mut() {
                let d = match f.kind {
                    "dibur_hamatchil" | "toc_partial" => None,
                    // a heading: the dibbur anywhere in its section ("ב"ח קא ד"ה …")
                    "toc" => {
                        let hi = toc
                            .as_deref()
                            .zip(f.heading)
                            .and_then(|(toc, heading)| {
                                toc.section_end(heading, part_words.as_deref())
                            })
                            .unwrap_or(i64::MAX);
                        dibur_in_lines(conn, caps, bid, f.line_index, hi, dh)?
                    }
                    _ => dibur_in_ref(conn, caps, index, bid, &used, f, dh)?,
                };
                if let Some(d) = d {
                    *f = d;
                }
            }
            // where the dibbur was found, the locations without it are not the reference
            if found.iter().any(|f| f.kind == "dibur_in_ref") {
                found.retain(|f| f.kind == "dibur_in_ref");
            }
        }
        // The global ranking uses the same order within a book. Bound path rendering
        // to the candidates that can survive that ranking.
        found.sort_by_key(|h| (kind_rank(h.kind), h.line_index));
        for hit in found.into_iter().take(limit.max(1) as usize) {
            out.push(RefCandidate {
                book_id: bid,
                book_title: title.clone(),
                line_index: hit.line_index,
                he_ref: hit.he_ref,
                toc_path: toc
                    .as_deref()
                    .zip(hit.heading)
                    .map(|(t, heading)| t.heading_path(heading)),
                snippet: String::new(),
                match_kind: hit.kind,
            });
        }
    }

    if out.is_empty() {
        return fallback_heref_like(conn, &ref_part, limit);
    }

    // Rank: best match kind first; base texts before commentaries; then catalogue order.
    out.sort_by_key(|c| {
        (
            kind_rank(c.match_kind),
            !index.is_base(c.book_id),
            index.order(c.book_id),
            c.book_id,
            c.line_index,
        )
    });
    out.truncate(limit.max(1) as usize);
    Ok(out)
}

struct Hit {
    line_index: i64,
    he_ref: Option<String>,
    kind: &'static str,
    heading: Option<TocHeading>,
}

impl Hit {
    fn new(line_index: i64, he_ref: Option<String>, kind: &'static str) -> Self {
        Self {
            line_index,
            he_ref,
            kind,
            heading: None,
        }
    }
}

type Found = Option<Hit>;

/// Canonical key → `line_ref` (the full key).
fn by_line_ref(conn: &Connection, caps: &Caps, book_id: i64, key: Option<&str>) -> Result<Found> {
    let Some(key) = key else { return Ok(None) };
    if !caps.line_ref {
        return Ok(None);
    }
    let row: Option<(i64, Option<String>)> = conn
        .query_row(
            "SELECT lr.lineIndex, l.heRef FROM line_ref lr \
               JOIN line l ON l.bookId = lr.bookId AND l.lineIndex = lr.lineIndex \
              WHERE lr.bookId = ?1 AND lr.refKeyHash = ?2 ORDER BY lr.lineIndex LIMIT 1",
            rusqlite::params![book_id, refkey::ref_key_hash(key)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row.map(|(li, he_ref)| Hit::new(li, he_ref, "exact")))
}

/// The partial key (named parts of the heRef omitted): every part of the book in which the
/// reference exists, one line each — the app's `resolvePartialRefKeyInBooks` (the part is
/// the heRef before the key's tokens; first line per part).
fn by_partial_line_ref(
    conn: &Connection,
    caps: &Caps,
    book_id: i64,
    key: &str,
) -> Result<Vec<Hit>> {
    if !caps.line_ref {
        return Ok(vec![]);
    }
    let partial = format!("{} {key}", refkey::PARTIAL_REF_KEY_MARKER);
    let mut stmt = conn.prepare(
        "SELECT lr.lineIndex, l.heRef FROM line_ref lr \
           JOIN line l ON l.bookId = lr.bookId AND l.lineIndex = lr.lineIndex \
          WHERE lr.bookId = ?1 AND lr.refKeyHash = ?2 ORDER BY lr.lineIndex",
    )?;
    let key_tokens = refkey::ref_key_tokens(key);
    let mut seen_parts = std::collections::HashSet::new();
    let mut out = Vec::new();
    for row in stmt.query_map(
        rusqlite::params![book_id, refkey::ref_key_hash(&partial)],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)),
    )? {
        let (li, he_ref) = row?;
        let Some(h) = &he_ref else { continue };
        let tokens = refkey::ref_key_tokens(h);
        // the omitted part is what precedes the key in the heRef; a heRef not ending
        // with the key (a range's start, say) counts as one unnamed part
        let part = if tokens.len() >= key_tokens.len()
            && tokens[tokens.len() - key_tokens.len()..] == key_tokens[..]
        {
            tokens[..tokens.len() - key_tokens.len()].join(" ")
        } else {
            String::new()
        };
        if seen_parts.insert(part) {
            out.push(Hit::new(li, he_ref, "partial"));
        }
    }
    Ok(out)
}

/// Normalized heRef: exact, else the first line the address is a token-prefix of.
fn by_heref(conn: &Connection, index: &RefIndex, book_id: i64, address: &str) -> Result<Found> {
    let book_href = index.book_heref.get(&book_id).cloned().unwrap_or_default();
    let mut canonical = normalize_ref(&book_href);
    if !address.is_empty() {
        canonical.push(' ');
        canonical.push_str(address);
    }
    let Some((line_index, he_ref)) = index.matching_line(conn, book_id, &canonical)? else {
        return Ok(None);
    };
    let kind = if normalize_ref(&he_ref) == canonical {
        "exact"
    } else {
        "prefix"
    };
    Ok(Some(Hit::new(line_index, Some(he_ref), kind)))
}

/// Try the alias's named section, exact headings, then the primary tree search.
fn by_toc_tree(
    toc: &BookToc,
    section: &[String],
    part_words: Option<&[String]>,
    toc_tokens: &[String],
    raw_address: &str,
) -> (Vec<Hit>, bool) {
    let collect = |headings: &[TocHeading], complete: bool| -> Vec<Hit> {
        let kind = if complete { "toc" } else { "toc_partial" };
        let mut seen = HashSet::new();
        headings
            .iter()
            .filter_map(|&heading| {
                let node = toc.node(heading);
                let line_index = node.line_index?;
                if part_words.is_some_and(|part| !toc.heading_in_part(heading, part))
                    || !seen.insert(line_index)
                {
                    return None;
                }
                Some(Hit {
                    line_index,
                    he_ref: node.he_ref.clone(),
                    kind,
                    heading: Some(heading),
                })
            })
            .collect()
    };
    if !section.is_empty() {
        let tokens: Vec<String> = section.iter().chain(toc_tokens).cloned().collect();
        let (headings, complete) = toc.search(&tokens);
        if complete {
            let hits = collect(&headings, true);
            if !hits.is_empty() {
                return (hits, true);
            }
        }
    }
    let hits = collect(toc.by_key(raw_address), true);
    if !hits.is_empty() {
        return (hits, true);
    }
    let (headings, complete) = toc.search(toc_tokens);
    (collect(&headings, complete), complete)
}

/// The heRef lies in the named part ("ב\"ח, יורה דעה, קא" in ["יורה", "דעה"]).
fn in_part(he_ref: &Option<String>, part: &[String]) -> bool {
    let Some(h) = he_ref else { return false };
    let h = format!(" {} ", normalize_ref(h));
    h.contains(&format!(" {} ", part.join(" ")))
}

/// The tail of the book name (as typed) that is not part of the book's title — an
/// alias carrying the part ("טור יו\"ד" → ["יוד"]). The app's `_acronymSectionTokens`.
/// Empty when the whole name is foreign to the title (a plain acronym) — then a last word
/// that looks like a part abbreviation ("ערוה\"ש יו\"ד") is the part.
fn acronym_section_tokens(name: &[String], own_words: &[String]) -> Vec<String> {
    let in_title = |t: &str| {
        own_words.iter().any(|w| {
            w == t
                || t.strip_prefix(['ה', 'ו']).is_some_and(|s| s == w)
                || w.strip_prefix(['ה', 'ו']).is_some_and(|s| s == t)
        })
    };
    let mut i = name.len();
    while i > 0 && !in_title(&name[i - 1]) {
        i -= 1;
    }
    if i > 0 {
        return name[i..].to_vec();
    }
    match name.last() {
        Some(last) if name.len() >= 2 && may_name_a_part(last) => vec![last.clone()],
        _ => vec![],
    }
}

/// A token that may abbreviate a part's name ("יוד", "אוח", "אהע", "חומ"): 3–4 letters,
/// not a number, not a locator word.
fn may_name_a_part(t: &str) -> bool {
    let n = t.chars().count();
    (3..=4).contains(&n)
        && t.chars().all(|c| ('\u{05D0}'..='\u{05EA}').contains(&c))
        && !refkey::is_hebrew_numeral(t)
        && !MARKER_WORDS.contains(&t)
}

fn join_nonempty(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (_, true) => a.to_string(),
        (true, false) => b.to_string(),
        _ => format!("{a} {b}"),
    }
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
    Ok(row.map(|(li, he_ref)| Hit::new(li, he_ref, "dibur_hamatchil")))
}

/// The first `ד"ה <words>` line in `lo..=hi` (a heading's section).
fn dibur_in_lines(
    conn: &Connection,
    caps: &Caps,
    book_id: i64,
    lo: i64,
    hi: i64,
    dibbur: &str,
) -> Result<Found> {
    if !caps.line_dh || dibbur.chars().count() < 2 {
        return Ok(None);
    }
    let row: Option<(i64, Option<String>)> = conn
        .query_row(
            "SELECT d.lineIndex, l.heRef FROM line_dh d \
               JOIN line l ON l.bookId = d.bookId AND l.lineIndex = d.lineIndex \
              WHERE d.bookId = ?1 AND d.lineIndex BETWEEN ?2 AND ?3 \
                AND d.dhText >= ?4 AND d.dhText < ?4 || char(1114111) \
              ORDER BY d.lineIndex LIMIT 1",
            rusqlite::params![book_id, lo, hi, dibbur],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row.map(|(li, he_ref)| Hit::new(li, he_ref, "dibur_in_ref")))
}

/// The `ד"ה <words>` line inside the location found for `address` (the lines whose heRef
/// the address is a prefix of; the found line itself when it has none below it).
fn dibur_in_ref(
    conn: &Connection,
    caps: &Caps,
    index: &RefIndex,
    book_id: i64,
    address: &str,
    found: &Hit,
    dibbur: &str,
) -> Result<Found> {
    if !caps.line_dh || dibbur.chars().count() < 2 {
        return Ok(None);
    }
    let book_href = index.book_heref.get(&book_id).cloned().unwrap_or_default();
    let mut canonical = normalize_ref(&book_href);
    if !address.is_empty() {
        canonical.push(' ');
        canonical.push_str(address);
    }
    // A partial key may omit a named part. Its own heRef keeps each
    // candidate within the part where it was found.
    let mut lines = if found.kind == "partial" {
        HashSet::new()
    } else {
        index.lines_under(conn, book_id, &canonical)?
    };
    if lines.is_empty() {
        if let Some(he_ref) = &found.he_ref {
            lines = index.lines_under(conn, book_id, &normalize_ref(he_ref))?;
        }
    }
    lines.insert(found.line_index);
    let lo = lines.iter().copied().min().unwrap_or(found.line_index);
    let hi = lines.iter().copied().max().unwrap_or(found.line_index);
    let mut stmt = conn.prepare(
        "SELECT d.lineIndex, l.heRef FROM line_dh d \
           JOIN line l ON l.bookId = d.bookId AND l.lineIndex = d.lineIndex \
          WHERE d.bookId = ?1 AND d.lineIndex BETWEEN ?2 AND ?3 \
            AND d.dhText >= ?4 AND d.dhText < ?4 || char(1114111) \
          ORDER BY d.lineIndex",
    )?;
    let rows = stmt.query_map(rusqlite::params![book_id, lo, hi, dibbur], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    for row in rows {
        let (li, he_ref) = row?;
        if lines.contains(&li) {
            return Ok(Some(Hit::new(li, he_ref, "dibur_in_ref")));
        }
    }
    Ok(None)
}

/// Words that only say what kind of number follows, after normalization (the canonical
/// key already drops some of them). None is a well-formed Hebrew numeral.
const MARKER_WORDS: &[&str] = &[
    "פרק", "הלכה", "סימן", "סי", "סעיף", "סע", "סק", "דף", "עמוד", "משנה", "אות",
];

/// Shulchan Aruch / Tur part names, written after a nosei kelim that covers one part.
const PART_NAMES: &[&str] = &[
    "אורח", "חיים", "אוח", "יורה", "דעה", "יוד", "אבן", "העזר", "אהע", "חושן", "משפט", "חומ",
];

/// Split off a trailing dibbur hamatchil: `... ד"ה ופורסין` → (`...`, Some(normalized words)).
fn split_dibbur(query: &str) -> (String, Option<String>) {
    let raw: Vec<&str> = query.split_whitespace().collect();
    match raw
        .iter()
        .position(|t| matches!(normalize_ref(t).as_str(), "דה" | "בדה"))
    {
        Some(i) => {
            let dh = normalize_ref(&raw[i + 1..].join(" "));
            (raw[..i].join(" "), (!dh.is_empty()).then_some(dh))
        }
        None => (query.to_string(), None),
    }
}

/// Rewrite an address the way a heRef spells it: unit markers dropped, marker-prefixed
/// numbers (`פ"ב`, `ה"א`, `מ"ג`, `סק"ב`, `פכ"ה`) reduced to the number.
fn clean_address(raw: &[&str]) -> Vec<String> {
    raw.iter()
        .filter(|tok| {
            let bare = normalize_ref(tok);
            !bare.is_empty() && !MARKER_WORDS.contains(&bare.as_str())
        })
        .map(|tok| strip_marker_prefix(tok).unwrap_or_else(|| tok.to_string()))
        .collect()
}

/// `פ"ב` → `ב`, `ה"א` → `א`, `מ"ג` → `ג`, `סק"ב` → `ב`, `פ"ק` → `א`, `פכ"ה` → `כ"ה`,
/// `הי"ב` → `י"ב`; `None` when the token has no such prefix.
fn strip_marker_prefix(tok: &str) -> Option<String> {
    let q = tok.find(['"', '\u{05F4}', '\'', '\u{05F3}'])?;
    let head = &tok[..q];
    let quote_len = tok[q..].chars().next()?.len_utf8();
    let tail = &tok[q + quote_len..];
    if ["פ", "ה", "מ", "סק", "ס"].contains(&head) && !tail.is_empty() {
        let n = normalize_ref(tail);
        if n == "ק" {
            return Some("א".to_string());
        }
        return refkey::is_hebrew_numeral(&n).then(|| tail.to_string());
    }
    let mut chars = head.chars();
    let first = chars.next()?;
    if (first == 'פ' || first == 'ה') && chars.next().is_some() {
        let rest = &tok[first.len_utf8()..];
        return refkey::is_hebrew_numeral(&normalize_ref(rest)).then(|| rest.to_string());
    }
    None
}

/// Drop the leading tokens of a normalized string while `drop` holds and the token is not
/// a number.
fn drop_leading(s: &str, drop: impl Fn(&str) -> bool) -> String {
    let toks: Vec<&str> = s.split_whitespace().collect();
    let skip = toks
        .iter()
        .take_while(|t| drop(t) && !refkey::is_hebrew_numeral(t))
        .count();
    toks[skip..].join(" ")
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
            toc_path: None,
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
             CREATE TABLE alt_toc_structure (id INTEGER PRIMARY KEY, bookId INTEGER NOT NULL);
             CREATE TABLE alt_toc_entry (id INTEGER PRIMARY KEY, structureId INTEGER NOT NULL, parentId INTEGER, textId INTEGER NOT NULL, level INTEGER NOT NULL, lineId INTEGER);
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

    /// A book printed as one tree over several parts — ב"ח (and טור, aliased "טור יו"ד"):
    /// "סימן קא" in both אורח חיים and יורה דעה.
    fn multipart_fixture() -> Connection {
        let conn = fixture();
        let mut line_id = 100;
        let mut toc_id = 100;
        for (book, title, alias) in [(50, "ב\"ח", None), (60, "טור", Some("טור יו\"ד"))]
        {
            conn.execute(
                "INSERT INTO book VALUES (?1, ?2, ?2, 0, 10)",
                rusqlite::params![book, title],
            )
            .unwrap();
            if let Some(a) = alias {
                conn.execute(
                    "INSERT INTO book_acronym VALUES (?1, ?2)",
                    rusqlite::params![book, a],
                )
                .unwrap();
            }
            for (part, base) in [("אורח חיים", 10), ("יורה דעה", 20)] {
                let part_toc = toc_id;
                for (offset, (siman, seif)) in
                    [("קא", "א"), ("קא", "ב"), ("קב", "א")].iter().enumerate()
                {
                    let li = base + offset as i64;
                    let he_ref = format!("{title}, {part}, {siman}, {seif}");
                    conn.execute(
                        "INSERT INTO line VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![line_id, book, li, he_ref],
                    )
                    .unwrap();
                    for key in [
                        format!("{part} {siman} {seif}"),
                        format!("~ {siman} {seif}"),
                    ] {
                        conn.execute(
                            "INSERT INTO line_ref VALUES (?1, ?2, ?3)",
                            rusqlite::params![book, crate::refkey::ref_key_hash(&key), li],
                        )
                        .unwrap();
                    }
                    if offset == 0 {
                        conn.execute(
                            "INSERT INTO tocText VALUES (?1, ?2)",
                            rusqlite::params![toc_id, part],
                        )
                        .unwrap();
                        conn.execute(
                            "INSERT INTO tocEntry VALUES (?1, ?2, NULL, ?1, 1, ?3, 1)",
                            rusqlite::params![toc_id, book, line_id],
                        )
                        .unwrap();
                        toc_id += 1;
                    }
                    if *seif == "א" {
                        conn.execute(
                            "INSERT INTO tocText VALUES (?1, ?2)",
                            rusqlite::params![toc_id, format!("סימן {siman}")],
                        )
                        .unwrap();
                        conn.execute(
                            "INSERT INTO tocEntry VALUES (?1, ?2, ?3, ?1, 2, ?4, 0)",
                            rusqlite::params![toc_id, book, part_toc, line_id],
                        )
                        .unwrap();
                        toc_id += 1;
                    }
                    line_id += 1;
                }
            }
        }
        conn
    }

    fn found(c: &[super::RefCandidate]) -> Vec<(i64, i64, &'static str)> {
        c.iter()
            .map(|c| (c.book_id, c.line_index, c.match_kind))
            .collect()
    }

    #[test]
    fn multipart_books_offer_every_part_unless_one_is_named() {
        let conn = multipart_fixture();
        let index = RefIndex::build(&conn);
        let r = |q: &str| found(&super::resolve(&conn, &caps(), &index, q, 10).unwrap());

        // a siman alone: the heading in every part (the app's 4 results for "ב"ח קא")
        assert_eq!(r("ב\"ח קא"), vec![(50, 10, "toc"), (50, 20, "toc")]);
        assert_eq!(r("ב\"ח סי' קא"), vec![(50, 10, "toc"), (50, 20, "toc")]);
        // the part named by its abbreviation, in the address or in the alias
        assert_eq!(r("ב\"ח יו\"ד קא"), vec![(50, 20, "prefix")]);
        assert_eq!(r("ב\"ח יורה דעה קא"), vec![(50, 20, "prefix")]);
        assert_eq!(r("טור יו\"ד קא"), vec![(60, 20, "prefix")]);
        // siman + seif: the partial key, one line per part …
        assert_eq!(
            r("ב\"ח קא ב"),
            vec![(50, 11, "partial"), (50, 21, "partial")]
        );
        // … or exactly the named part's line
        assert_eq!(r("ב\"ח יו\"ד קא ב"), vec![(50, 21, "exact")]);
        assert_eq!(r("ב\"ח או\"ח סי' קא סע' ב"), vec![(50, 11, "exact")]);
        assert_eq!(r("טור יו\"ד קב א"), vec![(60, 22, "exact")]);

        // the heading's path tells the parts apart
        let c = super::resolve(&conn, &caps(), &index, "ב\"ח קא", 10).unwrap();
        let paths: Vec<_> = c.iter().map(|c| c.toc_path.as_deref()).collect();
        assert_eq!(
            paths,
            vec![Some("אורח חיים > סימן קא"), Some("יורה דעה > סימן קא")]
        );

        // ד"ה after a siman: the part whose section has that dibbur
        conn.execute("INSERT INTO line_dh VALUES (50, 'ומש', 21, 'ומ\"ש')", [])
            .unwrap();
        assert_eq!(r("ב\"ח קא ד\"ה ומ\"ש"), vec![(50, 21, "dibur_in_ref")]);
        // … not past the section's end (סימן קב)
        assert_eq!(
            r("ב\"ח קב ד\"ה ומ\"ש")
                .iter()
                .filter(|c| c.2 == "dibur_in_ref")
                .count(),
            0
        );
    }

    #[test]
    fn named_parts_keep_headings_without_heref() {
        let conn = multipart_fixture();
        conn.execute("UPDATE line SET heRef = NULL WHERE bookId IN (50, 60)", [])
            .unwrap();
        let index = RefIndex::build(&conn);
        for (query, book_id) in [
            ("ב\"ח יו\"ד קא", 50),
            ("ב\"ח יורה דעה קא", 50),
            ("טור יו\"ד קא", 60),
        ] {
            let c = super::resolve(&conn, &caps(), &index, query, 10).unwrap();
            assert_eq!(found(&c), vec![(book_id, 20, "toc")], "{query}");
            assert_eq!(c[0].toc_path.as_deref(), Some("יורה דעה > סימן קא"));
        }
    }

    #[test]
    fn daf_toc_matches_page_and_side_positionally() {
        let conn = fixture();
        conn.execute_batch(
            "INSERT INTO line VALUES (50, 9, 10, NULL), (51, 9, 20, NULL);
             INSERT INTO tocText VALUES (50, 'דף ה:'), (51, 'דף ו:'), (52, 'כותרת לא מעוגנת');
             INSERT INTO tocEntry VALUES (50, 9, NULL, 50, 1, 50, 0), (51, 9, NULL, 51, 1, 51, 0),
                                         (52, 9, NULL, 52, 1, NULL, 0);",
        )
        .unwrap();
        let index = RefIndex::build(&conn);
        let r = |q: &str| found(&super::resolve(&conn, &caps(), &index, q, 10).unwrap());
        assert_eq!(r("רש\"י על ברכות ב:"), vec![]);
        assert_eq!(r("רש\"י על ברכות ה."), vec![]);
        assert_eq!(r("רש\"י על ברכות ה:"), vec![(9, 10, "toc")]);
        assert_eq!(r("רש\"י על ברכות ה ע\"ב"), vec![(9, 10, "toc")]);
        assert_eq!(r("רש\"י על ברכות כותרת לא מעוגנת"), vec![]);
    }

    #[test]
    fn alternate_toc_dibbur_stays_inside_its_structure_and_section() {
        let conn = fixture();
        conn.execute_batch(
            "INSERT INTO line VALUES (50, 103, 6, NULL), (51, 103, 12, NULL), (52, 103, 21, NULL);
             INSERT INTO tocText VALUES (50, 'פרשת נח'), (51, 'פרשת לך לך'), (52, 'מבנה שני');
             INSERT INTO alt_toc_structure VALUES (50, 103), (51, 103);
             INSERT INTO alt_toc_entry VALUES (50, 50, NULL, 50, 1, 2), (51, 50, NULL, 51, 1, 3),
                                              (52, 51, NULL, 52, 1, 50);
             INSERT INTO line_dh VALUES (103, 'בתוך הפרשה', 12, 'בתוך הפרשה'),
                                        (103, 'מחוץ לפרשה', 21, 'מחוץ לפרשה');",
        )
        .unwrap();
        let index = RefIndex::build(&conn);
        let caps = Caps {
            alt_toc: true,
            ..caps()
        };
        let r = |q: &str| super::resolve(&conn, &caps, &index, q, 10).unwrap();
        let c = r("ברכות פרשת נח ד\"ה מחוץ לפרשה");
        assert_eq!(found(&c), vec![(103, 2, "toc")]);
        assert_eq!(c[0].toc_path.as_deref(), Some("פרשת נח"));
        assert_eq!(
            found(&r("ברכות פרשת נח ד\"ה בתוך הפרשה")),
            vec![(103, 12, "dibur_in_ref")]
        );
    }

    #[test]
    fn named_part_bounds_alternate_toc_dibbur() {
        let conn = multipart_fixture();
        conn.execute_batch(
            "INSERT INTO book_acronym VALUES (60, 'טור או\"ח');
             INSERT INTO tocText VALUES (50, 'פרשת נח');
             INSERT INTO alt_toc_structure VALUES (50, 60);
             INSERT INTO alt_toc_entry SELECT 50, 50, NULL, 50, 1, id
               FROM line WHERE bookId = 60 AND lineIndex = 11;
             INSERT INTO line_dh VALUES (60, 'מחוץ לחלק', 21, 'מחוץ לחלק');",
        )
        .unwrap();
        let index = RefIndex::build(&conn);
        let caps = Caps {
            alt_toc: true,
            ..caps()
        };
        let c =
            super::resolve(&conn, &caps, &index, "טור או\"ח פרשת נח ד\"ה מחוץ לחלק", 10).unwrap();
        assert_eq!(found(&c), vec![(60, 11, "toc")]);
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

    /// heRefs and `line_ref` rows in the shapes of the live v32 library.
    fn citation_library() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE book (id INTEGER PRIMARY KEY, title TEXT NOT NULL, heRef TEXT, isBaseBook INTEGER NOT NULL, orderIndex INTEGER NOT NULL);
             CREATE TABLE book_acronym (bookId INTEGER NOT NULL, term TEXT NOT NULL);
             CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER NOT NULL, lineIndex INTEGER NOT NULL, heRef TEXT);
             CREATE TABLE line_ref (bookId INTEGER NOT NULL, refKeyHash INTEGER NOT NULL, lineIndex INTEGER NOT NULL);
             CREATE TABLE line_dh (bookId INTEGER NOT NULL, dhText TEXT NOT NULL, lineIndex INTEGER NOT NULL, dhDisplay TEXT NOT NULL);
             CREATE TABLE tocText (id INTEGER PRIMARY KEY, text TEXT NOT NULL);
             CREATE TABLE tocEntry (id INTEGER PRIMARY KEY, bookId INTEGER NOT NULL, parentId INTEGER, textId INTEGER NOT NULL, level INTEGER NOT NULL, lineId INTEGER, hasChildren INTEGER NOT NULL DEFAULT 0);
             INSERT INTO book VALUES
               (103, 'שבת', 'שבת', 1, 1),
               (144, 'תלמוד ירושלמי שביעית', 'תלמוד ירושלמי שביעית', 1, 2),
               (1907, 'תוספות על שבת', 'תוספות על שבת', 0, 3),
               (3839, 'מגן אברהם', 'מגן אברהם', 0, 4),
               (3862, 'קצות החושן על שולחן ערוך חושן משפט', 'קצות החושן על שולחן ערוך חושן משפט', 0, 5);
             INSERT INTO book_acronym VALUES
               (144, 'ירושלמי שביעית'), (1907, 'תוס'' שבת'), (1907, 'תוספות שבת'),
               (3839, 'מג\"א'), (3862, 'קצות'), (3862, 'קצות החושן חו\"מ');
             INSERT INTO line VALUES
               (1, 103, 500, 'שבת, מג:, א'),
               (2, 103, 900, 'שבת, פב., א'),
               (3, 144, 40, 'תלמוד ירושלמי שביעית, ב, א, א'),
               (4, 1907, 540, 'תוספות על שבת, מג:, א, א'),
               (5, 1907, 541, 'תוספות על שבת, מג:, ב, א'),
               (8, 1907, 560, 'תוספות על שבת, מד., א, א'),
               (6, 3839, 3267, 'מגן אברהם,  שח, ב'),
               (7, 3862, 171, 'קצות החושן על שולחן ערוך חושן משפט, כח, א, א');
             INSERT INTO line_dh VALUES
               (1907, 'דחשיב עלייהו', 540, 'דחשיב עלייהו'),
               (1907, 'בעודן עליו', 541, 'בעודן עליו'),
               (1907, 'בעודן עליו', 560, 'בעודן עליו');",
        )
        .unwrap();
        for (bid, li, addr) in [
            (103, 500, "מג:, א"),
            (103, 900, "פב., א"),
            (144, 40, "ב, א, א"),
            (1907, 540, "מג:, א, א"),
            (1907, 541, "מג:, ב, א"),
            (1907, 560, "מד., א, א"),
            (3839, 3267, "שח, ב"),
            (3862, 171, "כח, א, א"),
        ] {
            let key = crate::refkey::build_ref_key(addr).unwrap();
            conn.execute(
                "INSERT INTO line_ref VALUES (?1, ?2, ?3)",
                rusqlite::params![bid, crate::refkey::ref_key_hash(&key), li],
            )
            .unwrap();
        }
        conn
    }

    #[test]
    fn resolves_citation_forms_by_cleaned_address() {
        let conn = citation_library();
        let index = RefIndex::build(&conn);
        let first = |q: &str| {
            let c = super::resolve(&conn, &caps(), &index, q, 3).unwrap();
            assert!(!c.is_empty(), "no candidate for {q:?}");
            (c[0].book_id, c[0].line_index)
        };
        assert_eq!(first("ירושלמי שביעית פ\"ב ה\"א"), (144, 40));
        assert_eq!(first("מגן אברהם אורח חיים שח ב"), (3839, 3267));
        assert_eq!(first("מג\"א או\"ח סי' שח ס\"ק ב"), (3839, 3267));
        assert_eq!(first("מג\"א,סי' שח ס\"ק ב"), (3839, 3267));
        assert_eq!(first("קצות החושן כח א"), (3862, 171));
        assert_eq!(first("שבת דף מג ע\"ב"), (103, 500));
        // The address as typed still wins: daf 82, not perek 2.
        assert_eq!(first("שבת פ\"ב"), (103, 900));
    }

    #[test]
    fn dibbur_is_found_inside_the_location() {
        let conn = citation_library();
        let index = RefIndex::build(&conn);
        for query in ["תוס' שבת מג: ד\"ה בעודן", "תוס' שבת מג ע\"ב ד\"ה בעודן"]
        {
            let c = super::resolve(&conn, &caps(), &index, query, 3).unwrap();
            assert_eq!(
                (c[0].line_index, c[0].match_kind),
                (541, "dibur_in_ref"),
                "{query}"
            );
        }
        // The same dibbur on the next daf is not taken for this one.
        let c = super::resolve(&conn, &caps(), &index, "תוס' שבת מד. ד\"ה בעודן", 3).unwrap();
        assert_eq!((c[0].line_index, c[0].match_kind), (560, "dibur_in_ref"));
        // An unknown dibbur leaves the location itself.
        let c = super::resolve(&conn, &caps(), &index, "תוס' שבת מג: ד\"ה ופורסין", 3).unwrap();
        assert_eq!((c[0].line_index, c[0].match_kind), (540, "prefix"));
    }

    #[test]
    fn dibbur_can_match_later_line_with_the_same_heref() {
        let conn = citation_library();
        conn.execute("UPDATE line SET heRef = 'תוספות על שבת, מג:' WHERE bookId = 1907 AND lineIndex IN (540, 541)", [])
            .unwrap();
        let index = RefIndex::build(&conn);
        let c = super::resolve(&conn, &caps(), &index, "תוס' שבת מג: ד\"ה בעודן", 3).unwrap();
        assert_eq!(found(&c), vec![(1907, 541, "dibur_in_ref")]);
    }
}
