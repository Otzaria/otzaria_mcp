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

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use rusqlite::Connection;

use crate::refs::normalize_ref;

/// One TOC heading of a book.
#[derive(Debug, Clone)]
pub struct TocNode {
    pub id: i64,
    pub parent: Option<i64>,
    pub level: i64,
    pub line_index: i64,
    pub he_ref: Option<String>,
    /// The heading text (HTML stripped).
    pub text: String,
    /// Normalized tokens of the heading text itself.
    pub own: Vec<String>,
}

/// A book's TOC as a tree (level-0 entries — the book title — are left out, as in the app).
#[derive(Debug, Default)]
pub struct BookToc {
    pub nodes: Vec<TocNode>,
    roots: Vec<usize>,
    children: HashMap<i64, Vec<usize>>,
    by_id: HashMap<i64, usize>,
}

impl BookToc {
    pub fn load(conn: &Connection, book_id: i64) -> Result<BookToc> {
        let mut stmt = conn.prepare(
            "SELECT te.id, te.parentId, te.level, tt.text, l.lineIndex, l.heRef \
               FROM tocEntry te JOIN tocText tt ON tt.id = te.textId \
               LEFT JOIN line l ON l.id = te.lineId \
              WHERE te.bookId = ?1",
        )?;
        let mut raw = Vec::new();
        for row in stmt.query_map([book_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })? {
            raw.push(row?);
        }
        let level_of: HashMap<i64, i64> = raw.iter().map(|r| (r.0, r.2)).collect();
        let mut toc = BookToc::default();
        let mut nodes: Vec<TocNode> = raw
            .into_iter()
            .filter(|r| r.2 != 0)
            .map(|(id, parent, level, text, li, he_ref)| {
                let text = crate::text::strip_html_tags(&text).trim().to_string();
                TocNode {
                    id,
                    parent,
                    level,
                    line_index: li.unwrap_or(0),
                    he_ref,
                    own: normalize_ref(&text)
                        .split_whitespace()
                        .map(str::to_string)
                        .collect(),
                    text,
                }
            })
            .collect();
        nodes.sort_by_key(|n| (n.line_index, n.level, n.id));
        for (i, n) in nodes.iter().enumerate() {
            toc.by_id.insert(n.id, i);
            let parent_level = n.parent.and_then(|p| level_of.get(&p).copied());
            match (n.parent, parent_level) {
                (Some(p), Some(pl)) if pl != 0 => toc.children.entry(p).or_default().push(i),
                _ => toc.roots.push(i),
            }
        }
        toc.nodes = nodes;
        Ok(toc)
    }

    pub fn roots(&self) -> impl Iterator<Item = &TocNode> {
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

    /// Tokens of the heading path from the top part down to `i`.
    fn path_tokens(&self, i: usize) -> Vec<&str> {
        let mut chain = vec![i];
        let mut cur = self.nodes[i].parent;
        while let Some(p) = cur {
            match self.by_id.get(&p) {
                Some(&j) => {
                    chain.push(j);
                    cur = self.nodes[j].parent;
                }
                None => break,
            }
        }
        chain
            .iter()
            .rev()
            .flat_map(|&j| self.nodes[j].own.iter().map(String::as_str))
            .collect()
    }

    /// The deepest heading on `line_index`.
    fn heading_at(&self, line_index: i64) -> Option<usize> {
        (0..self.nodes.len())
            .filter(|&i| self.nodes[i].line_index == line_index)
            .max_by_key(|&i| self.nodes[i].level)
    }

    /// "אורח חיים > סימן קא": the path of the deepest heading on `line_index`.
    pub fn heading_path(&self, line_index: i64) -> Option<String> {
        let i = self.heading_at(line_index)?;
        let mut chain = vec![self.nodes[i].text.clone()];
        let mut cur = self.nodes[i].parent;
        while let Some(&j) = cur.and_then(|p| self.by_id.get(&p)) {
            chain.push(self.nodes[j].text.clone());
            cur = self.nodes[j].parent;
        }
        chain.reverse();
        Some(chain.join(" > "))
    }

    /// The last line of the section the heading on `line_index` opens (the line before the
    /// next heading at its level or above); `None` when it runs to the end of the book.
    pub fn section_end(&self, line_index: i64) -> Option<i64> {
        let i = self.heading_at(line_index)?;
        let level = self.nodes[i].level;
        self.nodes[i + 1..]
            .iter()
            .find(|n| n.line_index > line_index && n.level <= level)
            .map(|n| n.line_index - 1)
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
        (0..self.nodes.len())
            .filter(|&i| last_alts.iter().any(|a| self.nodes[i].own.contains(a)))
            .filter(|&i| {
                let path = self.path_tokens(i);
                tokens.iter().all(|t| {
                    token_alternatives(t)
                        .iter()
                        .any(|a| path.contains(&a.as_str()))
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
    pub fn spell_part(&self, part: &[String]) -> Option<Vec<String>> {
        if part.is_empty() {
            return None;
        }
        for n in self.roots() {
            if n.own.len() >= 2
                && !n.own.iter().any(|t| crate::refkey::is_hebrew_numeral(t))
                && (n.own.as_slice() == part
                    || (part.len() == 1
                        && !n.own.contains(&part[0])
                        && abbreviation_matches_words(&part[0], &n.own)))
            {
                return Some(n.own.clone());
            }
        }
        None
    }
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
    books: std::sync::Mutex<HashMap<i64, Arc<BookToc>>>,
}

impl TocCache {
    const MAX_BOOKS: usize = 256;

    pub fn get(&self, conn: &Connection, book_id: i64) -> Result<Arc<BookToc>> {
        if let Some(t) = self
            .books
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&book_id)
        {
            return Ok(t.clone());
        }
        let toc = Arc::new(BookToc::load(conn, book_id)?);
        let mut books = self.books.lock().unwrap_or_else(|p| p.into_inner());
        if books.len() >= Self::MAX_BOOKS {
            books.clear();
        }
        books.insert(book_id, toc.clone());
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

    fn toc_with_roots(headings: &[&str]) -> BookToc {
        let mut toc = BookToc::default();
        for (i, h) in headings.iter().enumerate() {
            toc.roots.push(i);
            toc.by_id.insert(i as i64, i);
            toc.nodes.push(TocNode {
                id: i as i64,
                parent: None,
                level: 1,
                line_index: i as i64,
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
        assert_eq!(toc.spell_part(&w("יוד")), Some(w("יורה דעה")));
        assert_eq!(toc.spell_part(&w("יורה דעה")), Some(w("יורה דעה")));
        assert_eq!(toc.spell_part(&w("מצוה")), None);
        assert_eq!(toc.spell_part(&w("אהע")), None);
    }

    #[test]
    fn two_letter_tokens_have_their_transposition() {
        assert_eq!(token_alternatives("טל"), vec!["טל", "לט"]);
        assert_eq!(token_alternatives("קא"), vec!["קא", "אק"]);
        assert_eq!(token_alternatives("קיא"), vec!["קיא"]);
    }
}
