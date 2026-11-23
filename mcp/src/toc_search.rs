//! TOC search for find-ref — a port of the app's `SeforimRepository.getTocEntriesForReference`
//! (`_searchTocHierarchically` + `_searchTocFlat`, lib/migration/database/repository/
//! seforim_repository.dart) and of the two token helpers it uses
//! (`hebrewTokenAlternatives`, `hebrewAbbreviationMatchesWords`, lib/utils/text/
//! text_manipulation.dart).
//!
//! The point of porting it rather than comparing a heading with the whole address: a book
//! printed as one tree over several parts (ב"ח, טור, ב"י, ערוך השולחן …) has the same
//! "סימן קא" heading in every part. The app walks the tree token by token — "יו"ד" matches
//! the part "יורה דעה" as an abbreviation, then "קא" its child — and returns *every*
//! heading that fits, so "ב"ח קא" offers all four parts instead of the first one.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use anyhow::Result;
use rusqlite::Connection;

use crate::{refkey, refs::normalize_ref};

/// One TOC heading of a book.
#[derive(Debug, Clone)]
pub struct TocNode {
    pub id: i64,
    pub parent: Option<i64>,
    pub level: i64,
    pub line_index: Option<i64>,
    pub he_ref: Option<String>,
    /// The heading text (HTML stripped).
    pub text: String,
    /// Normalized tokens of the heading text itself.
    pub own: Vec<String>,
}

/// A heading is identified within the structure that owns it, even if other
/// structures start a section on the same line.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TocHeading {
    structure: Option<i64>,
    index: usize,
}

/// A book's regular TOC and any alternate structures, with canonical heading lookup.
#[derive(Debug, Default)]
pub struct BookToc {
    primary: TocTree,
    alternate: BTreeMap<i64, TocTree>,
    by_key: HashMap<String, Vec<TocHeading>>,
}

#[derive(Debug, Default)]
struct TocTree {
    nodes: Vec<TocNode>,
    roots: Vec<usize>,
    children: HashMap<i64, Vec<usize>>,
    by_id: HashMap<i64, usize>,
}

impl BookToc {
    fn load(conn: &Connection, book_id: i64, alt_toc: bool) -> Result<Self> {
        let mut sql = String::from(
            "SELECT NULL, te.id, te.parentId, te.level, tt.text, l.lineIndex, l.heRef \
               FROM tocEntry te JOIN tocText tt ON tt.id = te.textId \
               LEFT JOIN line l ON l.id = te.lineId \
              WHERE te.bookId = ?1",
        );
        if alt_toc {
            sql.push_str(
                " UNION ALL SELECT ae.structureId, ae.id, ae.parentId, ae.level, tt.text, l.lineIndex, l.heRef \
                    FROM alt_toc_entry ae JOIN alt_toc_structure s ON s.id = ae.structureId \
                    JOIN tocText tt ON tt.id = ae.textId LEFT JOIN line l ON l.id = ae.lineId \
                   WHERE s.bookId = ?1",
            );
        }
        let mut stmt = conn.prepare(&sql)?;
        let mut structures: BTreeMap<Option<i64>, Vec<TocNode>> = BTreeMap::new();
        for row in stmt.query_map([book_id], |r| {
            let text = crate::text::strip_html_tags(&r.get::<_, String>(4)?)
                .trim()
                .to_string();
            let own = normalize_ref(&text)
                .split_whitespace()
                .map(str::to_string)
                .collect();
            Ok((
                r.get::<_, Option<i64>>(0)?,
                TocNode {
                    id: r.get(1)?,
                    parent: r.get(2)?,
                    level: r.get(3)?,
                    line_index: r.get(5)?,
                    he_ref: r.get(6)?,
                    text,
                    own,
                },
            ))
        })? {
            let (structure, node) = row?;
            structures.entry(structure).or_default().push(node);
        }
        let mut book = Self::default();
        for (structure, nodes) in structures {
            let tree = TocTree::new(nodes);
            for (index, node) in tree.nodes.iter().enumerate() {
                if let (Some(_), Some(key)) = (node.line_index, toc_key(&node.text)) {
                    book.by_key
                        .entry(key)
                        .or_default()
                        .push(TocHeading { structure, index });
                }
            }
            if let Some(id) = structure {
                book.alternate.insert(id, tree);
            } else {
                book.primary = tree;
            }
        }
        Ok(book)
    }

    pub(crate) fn by_key(&self, address: &str) -> &[TocHeading] {
        toc_key(address)
            .and_then(|key| self.by_key.get(&key))
            .map_or(&[], Vec::as_slice)
    }

    fn tree(&self, heading: TocHeading) -> &TocTree {
        match heading.structure {
            Some(id) => &self.alternate[&id],
            None => &self.primary,
        }
    }

    pub(crate) fn node(&self, heading: TocHeading) -> &TocNode {
        &self.tree(heading).nodes[heading.index]
    }

    pub(crate) fn heading_path(&self, heading: TocHeading) -> String {
        self.tree(heading).heading_path(heading.index)
    }

    pub(crate) fn section_end(&self, heading: TocHeading, part: Option<&[String]>) -> Option<i64> {
        let heading_end = self.tree(heading).section_end(heading.index);
        let part_end = part
            .and_then(|part| self.primary_part_at(heading, part))
            .and_then(|root| self.primary.section_end(root));
        heading_end.into_iter().chain(part_end).min()
    }

    pub(crate) fn heading_in_part(&self, heading: TocHeading, part: &[String]) -> bool {
        let tree = self.tree(heading);
        if tree.ancestors(heading.index).any(|n| n.own == part) {
            return true;
        }
        // Alternate structures can omit the part's name. When its regular TOC
        // heading has an anchor, use that part's line range.
        self.primary_part_at(heading, part).is_some()
    }

    fn primary_part_at(&self, heading: TocHeading, part: &[String]) -> Option<usize> {
        heading.structure?;
        let line = self.node(heading).line_index?;
        self.primary.roots.iter().copied().find(|&root| {
            let node = &self.primary.nodes[root];
            node.own == part
                && node.line_index.is_some_and(|start| start <= line)
                && self.primary.section_end(root).is_none_or(|end| line <= end)
        })
    }

    pub(crate) fn search(&self, tokens: &[String]) -> (Vec<TocHeading>, bool) {
        let (matches, complete) = self.primary.search(tokens);
        (
            matches
                .into_iter()
                .map(|index| TocHeading {
                    structure: None,
                    index,
                })
                .collect(),
            complete,
        )
    }

    pub(crate) fn named_part(&self, tokens: &[String]) -> Option<(Vec<String>, usize)> {
        self.primary.named_part(tokens)
    }
}

fn toc_key(text: &str) -> Option<String> {
    let key = refkey::build_ref_key(text)?;
    match key.strip_prefix("פרשת ") {
        Some(parasha) => Some(parasha.to_string()),
        None => Some(key),
    }
}

impl TocTree {
    fn new(mut nodes: Vec<TocNode>) -> Self {
        nodes.sort_by_key(|n| (n.line_index, n.level, n.id));
        let by_id: HashMap<i64, usize> = nodes.iter().enumerate().map(|(i, n)| (n.id, i)).collect();
        let mut toc = Self {
            by_id,
            ..Self::default()
        };
        for (i, n) in nodes.iter().enumerate() {
            if n.level == 0 {
                continue;
            }
            let parent_level = n
                .parent
                .and_then(|p| toc.by_id.get(&p))
                .map(|&j| nodes[j].level);
            match (n.parent, parent_level) {
                (Some(p), Some(pl)) if 0 < pl && pl < n.level => {
                    toc.children.entry(p).or_default().push(i)
                }
                _ => toc.roots.push(i),
            }
        }
        toc.nodes = nodes;
        toc
    }

    fn roots(&self) -> impl Iterator<Item = &TocNode> {
        self.roots.iter().map(|&i| &self.nodes[i])
    }

    fn descendants(&self, i: usize, out: &mut Vec<usize>) {
        if let Some(ch) = self.children.get(&self.nodes[i].id) {
            for &c in ch {
                out.push(c);
                self.descendants(c, out);
            }
        }
    }

    fn ancestors(&self, i: usize) -> impl Iterator<Item = &TocNode> {
        std::iter::successors(Some(&self.nodes[i]), |n| {
            let parent = n.parent.and_then(|p| self.by_id.get(&p))?;
            let node = &self.nodes[*parent];
            (0 < node.level && node.level < n.level).then_some(node)
        })
    }

    /// "אורח חיים > סימן קא": the path of the actual heading that matched.
    fn heading_path(&self, i: usize) -> String {
        let mut chain: Vec<&str> = self.ancestors(i).map(|n| n.text.as_str()).collect();
        chain.reverse();
        chain.join(" > ")
    }

    /// The last line of the section the heading opens (the line before the
    /// next heading at its level or above); `None` when it runs to the end of the book.
    fn section_end(&self, i: usize) -> Option<i64> {
        let line_index = self.nodes[i].line_index?;
        let level = self.nodes[i].level;
        self.nodes[i + 1..]
            .iter()
            .find(|n| n.line_index > Some(line_index) && 0 < n.level && n.level <= level)
            .and_then(|n| n.line_index)
            .map(|next| next - 1)
    }

    /// The app's hierarchical descent. `complete` is false when it stopped before the
    /// end of the query (the matches then are the deepest headings reached).
    pub fn search_hierarchically(&self, tokens: &[String]) -> (Vec<usize>, bool) {
        let mut scope: Vec<usize> = self.roots.clone();
        let mut current: Vec<usize> = Vec::new();
        for token in tokens {
            let alts = token_alternatives(token);
            let mut found: Vec<usize> = Vec::new();
            for alt in &alts {
                found = scope
                    .iter()
                    .copied()
                    .filter(|&i| self.nodes[i].own.iter().any(|t| t == alt))
                    .collect();
                if !found.is_empty() {
                    break;
                }
            }
            // An abbreviation of a multi-word heading ("יוד" ← "יורה דעה"), only after the
            // literal match failed, so that real location tokens are not caught by it.
            if found.is_empty() {
                found = scope
                    .iter()
                    .copied()
                    .filter(|&i| abbreviation_matches_words(token, &self.nodes[i].own))
                    .collect();
            }
            if found.is_empty() {
                // another word of a heading already matched ("חיים" after "אורח") does
                // not narrow, but must not drop the rest of the query
                if !current.is_empty()
                    && current
                        .iter()
                        .all(|&i| alts.iter().any(|a| self.nodes[i].own.contains(a)))
                {
                    continue;
                }
                return (current, false);
            }
            let min_level = found
                .iter()
                .map(|&i| self.nodes[i].level)
                .min()
                .unwrap_or(0);
            current = found
                .into_iter()
                .filter(|&i| self.nodes[i].level == min_level)
                .collect();
            let direct: Vec<usize> = current
                .iter()
                .flat_map(|&i| {
                    self.children
                        .get(&self.nodes[i].id)
                        .cloned()
                        .unwrap_or_default()
                })
                .collect();
            if direct.is_empty() {
                scope = current.clone();
            } else {
                let mut next = Vec::new();
                if current.len() > 1 {
                    next.extend(current.iter().copied());
                }
                for &c in &direct {
                    next.push(c);
                    self.descendants(c, &mut next);
                }
                scope = next;
            }
        }
        (current, true)
    }

    /// The app's flat fallback: the last token in the heading itself, every token somewhere
    /// on its path.
    pub fn search_flat(&self, tokens: &[String]) -> Vec<usize> {
        let Some(last) = tokens.last() else {
            return vec![];
        };
        let last_alts = token_alternatives(last);
        let citation = daf_citation(tokens);
        let alternatives: Vec<_> = tokens.iter().map(|t| token_alternatives(t)).collect();
        (0..self.nodes.len())
            .filter(|&i| self.nodes[i].level != 0)
            .filter(|&i| {
                let node = &self.nodes[i];
                if let (Some(daf), Some(citation)) = (heading_daf(&node.own), citation) {
                    return daf.matches(citation);
                }
                last_alts.iter().any(|a| node.own.contains(a))
                    && alternatives.iter().all(|alts| {
                        self.ancestors(i)
                            .any(|n| alts.iter().any(|a| n.own.contains(a)))
                    })
            })
            .collect()
    }

    /// `getTocEntriesForReference`: hierarchical, else flat. Returns (node indexes,
    /// complete); in line order.
    pub fn search(&self, tokens: &[String]) -> (Vec<usize>, bool) {
        if tokens.is_empty() {
            return (vec![], true);
        }
        if let Some(citation) = daf_citation(tokens) {
            let mut daf_headings = self
                .roots
                .iter()
                .copied()
                .filter_map(|i| heading_daf(&self.nodes[i].own).map(|daf| (i, daf)))
                .peekable();
            if daf_headings.peek().is_some() {
                let matches =
                    daf_headings.filter_map(|(i, daf)| daf.matches(citation).then_some(i));
                return (matches.collect(), true);
            }
        }
        let (mut hits, complete) = self.search_hierarchically(tokens);
        if !hits.is_empty() {
            hits.sort_by_key(|&i| self.nodes[i].line_index);
            return (hits, complete);
        }
        let mut flat = self.search_flat(tokens);
        flat.sort_by_key(|&i| self.nodes[i].line_index);
        (flat, true)
    }

    /// A part named by an abbreviation or by its own words, spelled out as the heading
    /// writes it ("יוד" → ["יורה", "דעה"]). Looked for among the top headings that are
    /// names — no number in them: "מצוה" is not an abbreviation of "מצוה ה".
    fn named_part(&self, tokens: &[String]) -> Option<(Vec<String>, usize)> {
        let first = tokens.first()?;
        self.roots()
            .filter(|n| n.own.len() >= 2 && !n.own.iter().any(|t| refkey::is_hebrew_numeral(t)))
            .find_map(|n| {
                if tokens.starts_with(&n.own) {
                    Some((n.own.clone(), n.own.len()))
                } else if !n.own.contains(first) && abbreviation_matches_words(first, &n.own) {
                    Some((n.own.clone(), 1))
                } else {
                    None
                }
            })
    }
}

#[derive(Clone, Copy)]
struct DafCitation<'a> {
    number: &'a str,
    amud: Option<char>,
}

impl DafCitation<'_> {
    fn matches(self, query: Self) -> bool {
        if query.amud.is_some_and(|amud| Some(amud) != self.amud) {
            return false;
        }
        if self.number == query.number {
            return true;
        }
        let mut letters = query.number.chars();
        match (letters.next(), letters.next(), letters.next()) {
            (Some(a), Some(b), None) => self.number.chars().eq([b, a]),
            _ => false,
        }
    }
}

fn daf_citation(tokens: &[String]) -> Option<DafCitation<'_>> {
    let mut location = tokens
        .iter()
        .map(String::as_str)
        .filter(|t| !matches!(*t, "דף" | "עמוד"));
    let number = location.next()?;
    if number.chars().count() > 3
        || !number
            .chars()
            .all(|c| ('\u{05D0}'..='\u{05EA}').contains(&c))
    {
        return None;
    }
    let amud = match location.next() {
        Some("א") => Some('א'),
        Some("ב") => Some('ב'),
        Some(_) => return None,
        None => None,
    };
    location
        .next()
        .is_none()
        .then_some(DafCitation { number, amud })
}

fn heading_daf(tokens: &[String]) -> Option<DafCitation<'_>> {
    let [label, number, rest @ ..] = tokens else {
        return None;
    };
    if label != "דף" {
        return None;
    }
    let amud = match rest.iter().map(String::as_str).find(|t| *t != "עמוד") {
        Some("א" | "עא") => Some('א'),
        Some("ב" | "עב") => Some('ב'),
        _ => None,
    };
    Some(DafCitation { number, amud })
}

/// `hebrewTokenAlternatives`: a two-letter Hebrew token and its transposition ("טל"/"לט").
pub fn token_alternatives(token: &str) -> Vec<String> {
    let chars: Vec<char> = token.chars().collect();
    let heb = |c: char| ('\u{05D0}'..='\u{05EA}').contains(&c);
    if chars.len() == 2 && heb(chars[0]) && heb(chars[1]) && chars[0] != chars[1] {
        return vec![token.to_string(), format!("{}{}", chars[1], chars[0])];
    }
    vec![token.to_string()]
}

/// `hebrewAbbreviationMatchesWords`: `token` is built from a non-empty prefix of each of
/// `words` in order ("יוד" ← "יורה דעה", "אהע" ← "אבן העזר"); at least 3 letters and 2 words.
pub fn abbreviation_matches_words(token: &str, words: &[String]) -> bool {
    let t: Vec<char> = token.chars().collect();
    if t.len() < 3 || words.len() < 2 {
        return false;
    }
    let ws: Vec<Vec<char>> = words.iter().map(|w| w.chars().collect()).collect();
    fn walk(t: &[char], ws: &[Vec<char>], wi: usize, pos: usize) -> bool {
        if wi == ws.len() {
            return pos == t.len();
        }
        let remaining = t.len().saturating_sub(pos);
        if remaining == 0 {
            return false;
        }
        let max_len = ws[wi].len().min(remaining);
        for len in 1..=max_len {
            if ws[wi][len - 1] != t[pos + len - 1] {
                break;
            }
            if walk(t, ws, wi + 1, pos + len) {
                return true;
            }
        }
        false
    }
    walk(&t, &ws, 0, 0)
}

/// Per-book TOC trees, loaded on first use.
#[derive(Default)]
pub struct TocCache {
    books: std::sync::Mutex<HashMap<(i64, bool), Arc<BookToc>>>,
}

impl TocCache {
    const MAX_BOOKS: usize = 256;

    pub fn get(&self, conn: &Connection, book_id: i64, alt_toc: bool) -> Result<Arc<BookToc>> {
        let key = (book_id, alt_toc);
        if let Some(t) = self
            .books
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
        {
            return Ok(t.clone());
        }
        let toc = Arc::new(BookToc::load(conn, book_id, alt_toc)?);
        let mut books = self.books.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(cached) = books.get(&key) {
            return Ok(cached.clone());
        }
        if books.len() >= Self::MAX_BOOKS {
            books.clear();
        }
        books.insert(key, toc.clone());
        Ok(toc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn abbreviations_follow_the_app() {
        assert!(abbreviation_matches_words("יוד", &w("יורה דעה")));
        assert!(abbreviation_matches_words("אהע", &w("אבן העזר")));
        assert!(abbreviation_matches_words("חומ", &w("חושן משפט")));
        assert!(abbreviation_matches_words("אוח", &w("אורח חיים")));
        assert!(!abbreviation_matches_words("יוד", &w("אורח חיים")));
        assert!(!abbreviation_matches_words("קא", &w("סימן קא")));
        assert!(!abbreviation_matches_words("יוד", &w("יורה")));
    }

    fn toc_with_roots(headings: &[&str]) -> TocTree {
        let mut toc = TocTree::default();
        for (i, h) in headings.iter().enumerate() {
            toc.roots.push(i);
            toc.by_id.insert(i as i64, i);
            toc.nodes.push(TocNode {
                id: i as i64,
                parent: None,
                level: 1,
                line_index: Some(i as i64),
                he_ref: None,
                text: h.to_string(),
                own: w(h),
            });
        }
        toc
    }

    #[test]
    fn parts_are_named_headings_only() {
        let toc = toc_with_roots(&["אורח חיים", "יורה דעה", "מצוה א", "מצוה ה"]);
        assert_eq!(toc.named_part(&w("יוד קא")), Some((w("יורה דעה"), 1)));
        assert_eq!(toc.named_part(&w("יורה דעה קא")), Some((w("יורה דעה"), 2)));
        assert_eq!(toc.named_part(&w("מצוה")), None);
        assert_eq!(toc.named_part(&w("אהע")), None);
    }

    #[test]
    fn two_letter_tokens_have_their_transposition() {
        assert_eq!(token_alternatives("טל"), vec!["טל", "לט"]);
        assert_eq!(token_alternatives("קא"), vec!["קא", "אק"]);
        assert_eq!(token_alternatives("קיא"), vec!["קיא"]);
    }
}
