//! The app's book detection for find-ref (`FindRefRepository._detectBooks` over
//! `BookTitleIndex.search`), used when no book NAME leads the query — "חזון איש (אורח חיים
//! קכא:ג)" names no book exactly, but its words begin the title "חזון איש, אורח חיים מועד".
//!
//! Ported: the app's normalizer (`normalizeForFindRefMatch`), the comparison forms of book
//! names (`bookNameMatchForm`: וו/יי as one letter, no ה"א before a sage's acronym), the
//! match ranks of a title (0 equal, 1 prefix, 2 contains) and of an acronym (3 equal,
//! 4 prefix, 5 contains), the phrase loop (the longest leading phrase of up to three
//! words with a direct match wins; "contains" matches ride along from every length), the
//! שו"ת prefix retry, and the remaining-address computation (`_getRemainingTokens`).
//! The approximate (edit-distance) title match is ported too ("המאירי" → "מאירי על …").
//! Not ported: folder-name matching of personal books, and PDFs outside the databases.

use std::collections::HashMap;

/// `normalizeForFindRefMatch`: no nikud/te'amim/gershayim/punctuation; "ב." → "ב א" and
/// "ב:" → "ב ב" after a run of 1–3 letters not preceded by a quote (so "פ"א." stays).
pub fn normalize_find_ref(input: &str) -> String {
    let units: Vec<char> = input
        .chars()
        .filter_map(|c| match c {
            '\u{05BE}' | '\u{05C0}' | '|' => Some(' '),
            '\u{0591}'..='\u{05C7}' => None,
            _ => Some(c),
        })
        .collect();
    let n = units.len();
    let mut out = String::with_capacity(input.len() + 8);
    let mut pending_space = false;
    let emit = |c: char, out: &mut String, pending: &mut bool| {
        if is_quote(c) {
            return;
        }
        let c = c.to_ascii_lowercase();
        let ok =
            c.is_ascii_lowercase() || c.is_ascii_digit() || ('\u{0590}'..='\u{05FF}').contains(&c);
        if !ok {
            *pending = true;
            return;
        }
        if *pending {
            if !out.is_empty() {
                out.push(' ');
            }
            *pending = false;
        }
        out.push(c);
    };
    let mut i = 0;
    while i < n {
        let c = units[i];
        if !is_hebrew_letter(c) {
            emit(c, &mut out, &mut pending_space);
            i += 1;
            continue;
        }
        let mut end = i + 1;
        while end < n && is_hebrew_letter(units[end]) {
            end += 1;
        }
        for &u in &units[i..end] {
            emit(u, &mut out, &mut pending_space);
        }
        if end - i <= 3
            && end < n
            && (units[end] == '.' || units[end] == ':')
            && (end + 1 == n || units[end + 1].is_whitespace())
            && (i == 0 || !is_quote(units[i - 1]))
        {
            pending_space = true;
            emit(
                if units[end] == '.' { 'א' } else { 'ב' },
                &mut out,
                &mut pending_space,
            );
            end += 1;
        }
        i = end;
    }
    out
}

fn is_hebrew_letter(c: char) -> bool {
    ('\u{05D0}'..='\u{05EA}').contains(&c)
}

fn is_quote(c: char) -> bool {
    matches!(c, '"' | '\'' | '\u{05F3}' | '\u{05F4}')
}

/// Acronyms the definite article comes before ("הרמב"ם"), as in the app's
/// `sagesTakingDefiniteArticle`.
const SAGES_WITH_ARTICLE: &[&str] = &[
    "רמבם",
    "רמבן",
    "ראש",
    "רשבא",
    "רשבם",
    "ריטבא",
    "רדק",
    "רלבג",
    "ריף",
    "ראבד",
    "ראבע",
    "רן",
    "רש",
    "רי",
    "רסג",
    "ראה",
    "ריבא",
    "ריד",
    "רדבז",
    "רמה",
    "רמך",
    "רזה",
    "ריאז",
    "ריץ",
    "רמא",
    "טז",
    "שך",
    "בח",
    "סמע",
    "רעב",
    "מביט",
    "חידא",
    "יעבץ",
    "תשבץ",
    "מלבים",
    "רמק",
    "רמחל",
    "רשש",
    "רידבז",
    "אדרת",
    "נציב",
    "רדל",
    "רימ",
    "ראיה",
    "ארי",
    "אריזל",
    "מב",
    "רשז",
    "מהרם",
    "מהרל",
    "מהרי",
    "מהרץ",
    "מהרש",
    "מהרשא",
    "מהרשל",
    "מהרשך",
    "מהרשם",
    "מהריל",
    "מהריק",
    "מהריט",
    "מהרחו",
    "מהראל",
    "מהרזו",
    "מהריו",
    "גרא",
    "גרעא",
    "גרח",
    "גרשז",
    "גריז",
    "גרישא",
    "גרשש",
    "גריד",
    "גראל",
    "גרש",
    "גרנט",
    "גרמפ",
    "גריב",
    "גרמ",
    "גריש",
    "גרעקא",
    "גרשזא",
    "שס",
    "תנך",
    "נך",
    "קבה",
];

/// `bookNameSpellingForm`: double vav / yod as one letter.
pub fn spelling_form(text: &str) -> String {
    if text.contains("וו") || text.contains("יי") {
        text.replace("וו", "ו").replace("יי", "י")
    } else {
        text.to_string()
    }
}

/// `bookNameMatchToken`: spelling form, and no ה"א before a sage's acronym.
pub fn match_token(token: &str) -> String {
    let t = spelling_form(token);
    if t.chars().count() > 2 {
        if let Some(rest) = t.strip_prefix('ה') {
            if SAGES_WITH_ARTICLE.contains(&rest) {
                return rest.to_string();
            }
        }
    }
    t
}

/// `bookNameMatchForm` of normalized, single-spaced text.
pub fn match_form(text: &str) -> String {
    if !text.contains("וו") && !text.contains("יי") && !text.contains('ה') {
        return text.to_string();
    }
    text.split(' ')
        .map(match_token)
        .collect::<Vec<_>>()
        .join(" ")
}

/// `titleTokenWithoutConjunction`: a title word without a leading ה"א (or ו, not first).
pub fn without_conjunction(token: &str, allow_vav: bool) -> Option<&str> {
    if token.chars().count() < 3 {
        return None;
    }
    if let Some(r) = token.strip_prefix('ה') {
        return Some(r);
    }
    if allow_vav {
        if let Some(r) = token.strip_prefix('ו') {
            return Some(r);
        }
    }
    None
}

struct TitleBook {
    id: i64,
    order: i64,
    title_len: usize,
    /// Match form of the normalized title.
    mt: String,
    tokens: Vec<String>,
    /// Acronyms in their spelling form.
    acronyms: Vec<String>,
    /// The acronyms' words (for the approximate match).
    acronym_tokens: Vec<Vec<String>>,
}

/// The approximate (typo-tolerant) title match ranks below every literal one.
pub const FUZZY_RANK: u8 = 6;

/// `_fuzzyMatchedTerm`: every query word is in the text — a word of under three letters
/// as a whole word, a longer one contained in a word or within the edit distance the
/// longer of the two allows.
fn fuzzy_all_words_in(words: &[&str], text: &[String]) -> bool {
    if !words.iter().any(|w| w.chars().count() >= 3) {
        return false;
    }
    words.iter().all(|w| {
        if w.chars().count() < 3 {
            text.iter().any(|t| t == w)
        } else {
            text.iter().any(|t| word_pair_matches(w, t))
        }
    })
}

/// `_wordPairMatches`.
fn word_pair_matches(q: &str, t: &str) -> bool {
    if t.contains(q) {
        return true;
    }
    let (qc, tc): (Vec<char>, Vec<char>) = (q.chars().collect(), t.chars().collect());
    if qc.len() < 3 {
        return false;
    }
    if (q.contains("וו") || q.contains("יי") || t.contains("וו") || t.contains("יי"))
        && spelling_form(t).contains(&spelling_form(q))
    {
        return true;
    }
    // plene and defective spelling (יושר/ישר, מידות/מדות): the same without inner ו/י
    let bare = |w: &[char]| -> String {
        w.iter()
            .enumerate()
            .filter(|(i, c)| *i == 0 || *i + 1 == w.len() || !matches!(c, 'ו' | 'י'))
            .map(|(_, c)| *c)
            .collect()
    };
    if qc.len() >= 3 && tc.len() >= 3 && bare(&qc) == bare(&tc) {
        return true;
    }
    let allowed = match qc.len().max(tc.len()) {
        0..=4 => 0,
        5..=8 => 1,
        9..=12 => 2,
        13..=16 => 3,
        _ => 4,
    };
    if allowed == 0 || qc.len().abs_diff(tc.len()) > allowed {
        return false;
    }
    edit_distance_at_most(&qc, &tc, allowed)
}

/// A misspelt name word, for scoring: within the edit distance, the same in plene and
/// defective spelling, or the end of the title word less one letter ("לטי" of "שלטי") —
/// not any word it happens to be part of ("ישר" of "ישראל").
fn name_word_near(q: &str, t: &str) -> bool {
    let (qn, tn) = (q.chars().count(), t.chars().count());
    if t.contains(q) {
        return t.ends_with(q) && tn - qn <= 1;
    }
    word_pair_matches(q, t)
}

/// Damerau (adjacent transposition) edit distance ≤ k.
fn edit_distance_at_most(a: &[char], b: &[char], k: usize) -> bool {
    let (la, lb) = (a.len(), b.len());
    let mut prev2 = vec![0usize; lb + 1];
    let mut prev: Vec<usize> = (0..=lb).collect();
    let mut curr = vec![0usize; lb + 1];
    let mut prev_min = 0;
    for i in 1..=la {
        curr[0] = i;
        let mut row_min = i;
        for j in 1..=lb {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (prev[j - 1] + cost).min(prev[j] + 1).min(curr[j - 1] + 1);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(prev2[j - 2] + cost);
            }
            curr[j] = best;
            row_min = row_min.min(best);
        }
        if row_min > k && prev_min > k {
            return false;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut curr);
        prev_min = row_min;
    }
    prev[lb] <= k
}

/// A book a phrase of the query names.
#[derive(Debug, Clone)]
pub struct BookHit {
    pub id: i64,
    /// 0 title equal, 1 title prefix, 2 title contains, 3 acronym equal, 4 acronym
    /// prefix, 5 acronym contains, 6 approximate.
    pub rank: u8,
    /// Normalized title tokens.
    pub tokens: Vec<String>,
    /// How many leading query tokens named it.
    pub phrase_len: usize,
    order: i64,
    title_len: usize,
}

/// The books of one database, for the app's title / acronym matching.
pub struct TitleIndex {
    books: Vec<TitleBook>,
}

/// The app's `bookSearchLimit` for multi-word queries.
const SEARCH_LIMIT: usize = 1000;

impl TitleIndex {
    pub fn build<'a>(
        books: impl Iterator<Item = (i64, &'a str, i64)>,
        acronyms: &HashMap<i64, Vec<String>>,
    ) -> TitleIndex {
        let mut out = Vec::new();
        for (id, title, order) in books {
            let norm = normalize_find_ref(title);
            if norm.is_empty() {
                continue;
            }
            let acr_norm: Vec<String> = acronyms
                .get(&id)
                .map(|v| {
                    v.iter()
                        .map(|a| normalize_find_ref(a))
                        .filter(|a| !a.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let acronym_tokens = acr_norm
                .iter()
                .map(|a| a.split(' ').map(str::to_string).collect())
                .collect();
            let acr = acr_norm.iter().map(|a| spelling_form(a)).collect();
            out.push(TitleBook {
                id,
                order,
                title_len: title.chars().count(),
                mt: match_form(&norm),
                tokens: norm.split(' ').map(str::to_string).collect(),
                acronyms: acr,
                acronym_tokens,
            });
        }
        TitleIndex { books: out }
    }

    /// `BookTitleIndex.search` (without the approximate match), best first.
    pub fn search(&self, phrase: &str, phrase_len: usize) -> Vec<BookHit> {
        let q = normalize_find_ref(phrase);
        if q.is_empty() {
            return vec![];
        }
        let mq = match_form(&q);
        let sq = spelling_form(&q);
        let forms: Vec<&str> = if mq != sq {
            vec![sq.as_str(), mq.as_str()]
        } else {
            vec![sq.as_str()]
        };
        let words: Vec<&str> = q.split(' ').collect();
        let mut hits = Vec::new();
        for b in &self.books {
            let mut rank: Option<u8> = if b.mt == mq {
                Some(0)
            } else if b.mt.starts_with(&mq) {
                Some(1)
            } else if b.mt.contains(&mq) {
                Some(2)
            } else {
                None
            };
            if rank.is_none() {
                for a in &b.acronyms {
                    if forms.iter().any(|f| a == f) {
                        rank = Some(3);
                        break;
                    }
                    if forms.iter().any(|f| a.starts_with(f)) {
                        if rank.is_none_or(|r| r > 4) {
                            rank = Some(4);
                        }
                    } else if rank.is_none() && forms.iter().any(|f| a.contains(f)) {
                        rank = Some(5);
                    }
                }
            }
            if rank.is_none() && fuzzy_all_words_in(&words, &b.tokens) {
                rank = Some(FUZZY_RANK);
            }
            if rank.is_none() {
                for a in &b.acronym_tokens {
                    if fuzzy_all_words_in(&words, a) {
                        rank = Some(FUZZY_RANK);
                        break;
                    }
                }
            }
            if let Some(rank) = rank {
                hits.push(BookHit {
                    id: b.id,
                    rank,
                    tokens: b.tokens.clone(),
                    phrase_len,
                    order: b.order,
                    title_len: b.title_len,
                });
            }
        }
        hits.sort_by_key(|h| (h.rank, h.order, h.title_len));
        hits.truncate(SEARCH_LIMIT);
        hits
    }

    /// `_detectBooks`: the books the query mentions and how many leading tokens named
    /// them (secondary "contains" hits keep their own phrase length).
    pub fn detect(&self, tokens: &[String], max_phrase: usize) -> (Vec<BookHit>, usize) {
        let max_phrase = tokens.len().min(max_phrase).max(1);
        let mut book_hits: Vec<BookHit> = Vec::new();
        let mut phrase_len = 1;
        let mut secondary: Vec<BookHit> = Vec::new();
        for n in (1..=max_phrase).rev() {
            let phrase_tokens = &tokens[..n];
            let hits = self.search(&phrase_tokens.join(" "), n);
            if hits.is_empty() {
                continue;
            }
            if n == 1 {
                book_hits = hits;
                phrase_len = 1;
                break;
            }
            let mut primary = Vec::new();
            for h in hits {
                if h.rank == 3 {
                    primary.push(h);
                    continue;
                }
                if h.rank == FUZZY_RANK {
                    secondary.push(h);
                    continue;
                }
                if h.rank >= 4 || !phrase_appears_as_tokens(&h.tokens, phrase_tokens) {
                    continue;
                }
                if h.rank == 2 {
                    secondary.push(h);
                } else {
                    primary.push(h);
                }
            }
            if !primary.is_empty() {
                book_hits = primary;
                phrase_len = n;
                break;
            }
        }
        // A book also found by a longer phrase keeps the longer one: more of the query
        // is its name ("השערי יושר": both words, not only "השערי").
        let mut by_id: HashMap<i64, usize> = HashMap::new();
        for (i, h) in book_hits.iter().enumerate() {
            by_id.entry(h.id).or_insert(i);
        }
        for h in secondary {
            match by_id.get(&h.id) {
                Some(&i) => {
                    if h.phrase_len > book_hits[i].phrase_len {
                        book_hits[i] = h;
                    }
                }
                None => {
                    by_id.insert(h.id, book_hits.len());
                    book_hits.push(h);
                }
            }
        }
        (book_hits, phrase_len)
    }
}

/// `_phraseAppearsAsTokens`: the phrase's words begin consecutive title words.
fn phrase_appears_as_tokens(title: &[String], phrase: &[String]) -> bool {
    if phrase.is_empty() {
        return true;
    }
    if phrase.len() > title.len() {
        return false;
    }
    let phrase: Vec<String> = phrase.iter().map(|t| match_token(t)).collect();
    (0..=title.len() - phrase.len()).any(|start| {
        phrase
            .iter()
            .enumerate()
            .all(|(i, p)| match_token(&title[start + i]).starts_with(p.as_str()))
    })
}

/// `_getRemainingTokens`: the query without the words of the title (each title word
/// takes one query word, wherever it stands), after dropping `strip` leading tokens
/// (an acronym's). Only the first `prefix` query tokens may match a title word by prefix.
pub fn remaining_tokens(
    query: &[String],
    title: &[String],
    strip: usize,
    prefix: usize,
) -> Vec<String> {
    let mut remaining: Vec<String> = query[strip.min(query.len())..].to_vec();
    let eligible: Vec<&String> = query.iter().take(prefix).collect();
    for (ti, token) in title.iter().enumerate() {
        let mut idx = remaining.iter().position(|t| t == token);
        if idx.is_none() {
            if let Some(bare) = without_conjunction(token, ti > 0) {
                idx = remaining.iter().position(|t| t == bare);
            }
        }
        if idx.is_none() && !eligible.is_empty() {
            idx = remaining.iter().position(|t| {
                t.chars().count() >= 2 && eligible.contains(&t) && token.starts_with(t.as_str())
            });
        }
        if idx.is_none() {
            let mt = match_token(token);
            idx = remaining.iter().position(|t| {
                t.chars().count() >= 2
                    && (match_token(t) == mt
                        || (eligible.contains(&t) && mt.starts_with(match_token(t).as_str())))
            });
        }
        if let Some(i) = idx {
            remaining.remove(i);
        }
    }
    remaining
}

/// Words a citation adds around a location that no heading spells ("חו"מ צ, בסופו").
const CITATION_FILLER: &[&str] = &[
    "בסופו",
    "בסופה",
    "בסוף",
    "בתחילתו",
    "בתחלתו",
    "בתחילתה",
    "בתחלתה",
    "בריש",
    "ריש",
    "ועיין",
    "ועי",
    "עיין",
    "עי",
    "וכו",
    "וכן",
    "ועוד",
];

/// What the app's flat TOC search lets pass because it matches the query against the
/// whole reference, title included: the title's words wherever they stand (also a
/// repeated one), a part abbreviation of title words ("חו"מ" in "… חושן משפט"), a
/// "ח"X" whose "חלק X" is in the title; and citation filler ("בסופו").
pub fn drop_title_words(
    remaining: Vec<String>,
    title: &[String],
    name_words: &[String],
) -> Vec<String> {
    let forms: Vec<String> = title
        .iter()
        .enumerate()
        .flat_map(|(i, t)| {
            let mut v = vec![t.clone(), match_token(t)];
            if let Some(b) = without_conjunction(t, i > 0) {
                v.push(b.to_string());
            }
            v
        })
        .collect();
    let parts: Vec<String> = title
        .windows(2)
        .filter(|w| w[0] == "חלק")
        .map(|w| format!("ח{}", w[1]))
        .collect();
    remaining
        .into_iter()
        .filter(|t| {
            let n = t.chars().count();
            if CITATION_FILLER.contains(&t.as_str()) {
                return false;
            }
            if n >= 2 && (forms.contains(t) || forms.contains(&match_token(t))) {
                return false;
            }
            if parts.contains(t) {
                return false;
            }
            // a word of the phrase that named the book, misspelt ("לטי הגבורים")
            if n >= 3
                && name_words.contains(t)
                && title
                    .iter()
                    .any(|w| word_pair_matches(&match_token(t), &match_token(w)))
            {
                return false;
            }
            !(n >= 3
                && (0..title.len().saturating_sub(1)).any(|i| {
                    (i + 2..=title.len().min(i + 3))
                        .any(|j| crate::toc_search::abbreviation_matches_words(t, &title[i..j]))
                }))
        })
        .collect()
}

/// `_titleCoverage`: the query's words of two letters or more that are a title word
/// (or one without its ה/ו) — which books the app looks into first.
pub fn title_coverage(title: &[String], query: &[String]) -> usize {
    query
        .iter()
        .filter(|q| q.chars().count() >= 2)
        .filter(|q| {
            title
                .iter()
                .enumerate()
                .any(|(i, t)| t == *q || without_conjunction(t, i > 0) == Some(q.as_str()))
        })
        .count()
}

/// How well `words` (a title, a reference) account for the query's words of two letters
/// or more: 2 for a word itself (also without a leading ה/ו, or in the comparison form),
/// 1 for a misspelling within the approximate match's tolerance. Also returns how many
/// of `words` were used.
pub fn match_score(words: &[String], query: &[String]) -> (usize, usize) {
    let forms: Vec<(String, Option<String>, String)> = words
        .iter()
        .enumerate()
        .map(|(i, w)| {
            (
                w.clone(),
                without_conjunction(w, i > 0).map(str::to_string),
                match_token(w),
            )
        })
        .collect();
    let mut used = vec![false; forms.len()];
    let mut score = 0;
    let mut seen: Vec<&String> = Vec::new();
    for q in query.iter().filter(|q| q.chars().count() >= 2) {
        if seen.contains(&q) {
            continue;
        }
        seen.push(q);
        let mq = match_token(q);
        let bare_q = without_conjunction(q, true);
        let exact = forms.iter().enumerate().position(|(i, (w, b, m))| {
            !used[i]
                && (w == q
                    || b.as_deref() == Some(q.as_str())
                    || *m == mq
                    || bare_q == Some(w.as_str()))
        });
        if let Some(i) = exact {
            used[i] = true;
            score += 2;
            continue;
        }
        if q.chars().count() >= 3 {
            let near = forms
                .iter()
                .enumerate()
                .position(|(i, (_, _, m))| !used[i] && name_word_near(&mq, m));
            if let Some(i) = near {
                used[i] = true;
                score += 1;
            }
        }
    }
    (score, used.iter().filter(|u| **u).count())
}

/// A colon or period inside a raw word is an amud mark only where the app reads one
/// (after a run of 1–3 letters, at the word's end); elsewhere ("קכא:ג", "כג:ב") it
/// separates two numbers.
pub fn split_inner_marks(word: &str) -> String {
    let chars: Vec<char> = word.chars().collect();
    let mut out = String::with_capacity(word.len());
    let mut run = 0usize;
    let mut run_after_quote = false;
    for (i, &c) in chars.iter().enumerate() {
        if c == ':' || c == '.' {
            let at_end = chars[i + 1..]
                .iter()
                .all(|&d| matches!(d, ')' | ']' | ',' | ';'));
            if (1..=3).contains(&run) && at_end && !run_after_quote {
                out.push(c);
            } else {
                out.push(' ');
            }
            run = 0;
            continue;
        }
        if is_hebrew_letter(c) {
            if run == 0 {
                run_after_quote = i > 0 && is_quote(chars[i - 1]);
            }
            run += 1;
        } else {
            run = 0;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn normalizer_follows_the_app() {
        assert_eq!(
            normalize_find_ref("חזון איש (אורח חיים קכא:ג)"),
            "חזון איש אורח חיים קכא ג"
        );
        assert_eq!(normalize_find_ref("ברכות ב."), "ברכות ב א");
        assert_eq!(normalize_find_ref("שבת קלו:"), "שבת קלו ב");
        assert_eq!(normalize_find_ref("הר\"ן (גיטין עה,ב)"), "הרן גיטין עה ב");
        assert_eq!(normalize_find_ref("פ\"א."), "פא");
        assert_eq!(normalize_find_ref("שו\"ע"), "שוע");
    }

    #[test]
    fn match_forms() {
        assert_eq!(match_form("הרמבם על משנה"), "רמבם על משנה");
        assert_eq!(match_form("מקוואות"), "מקואות");
        assert_eq!(match_form("הלכות"), "הלכות");
    }

    #[test]
    fn detection_prefers_a_leading_phrase() {
        let titles = [
            (1, "חזון איש, אורח חיים מועד", 1),
            (2, "חזון איש, יורה דעה", 2),
            (3, "ספר אחר על חזון איש", 3),
        ];
        let ix = TitleIndex::build(titles.iter().map(|(i, t, o)| (*i, *t, *o)), &HashMap::new());
        let (hits, n) = ix.detect(&w("חזון איש אורח חיים קכא ג"), 3);
        assert_eq!(n, 3);
        let ids: Vec<i64> = hits.iter().map(|h| h.id).collect();
        assert_eq!(ids, vec![1]);
        let (hits, n) = ix.detect(&w("חזון איש קכא ג"), 3);
        assert_eq!(n, 2);
        let ids: Vec<i64> = hits.iter().map(|h| h.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        let rem = remaining_tokens(&w("חזון איש אורח חיים קכא ג"), &hits[0].tokens, 0, 3);
        assert_eq!(rem, w("קכא ג"));
    }

    #[test]
    fn acronym_hits_strip_their_phrase() {
        let mut acr = HashMap::new();
        acr.insert(7, vec!["קצוה\"ח".to_string()]);
        let ix = TitleIndex::build([(7, "קצות החושן", 1)].into_iter(), &acr);
        let (hits, n) = ix.detect(&w("קצוהח רמא א"), 3);
        assert_eq!((hits[0].id, hits[0].rank, n), (7, 3, 1));
        let rem = remaining_tokens(&w("קצוהח רמא א"), &hits[0].tokens, 1, 0);
        assert_eq!(rem, w("רמא א"));
    }

    #[test]
    fn inner_colons_split_numbers() {
        assert_eq!(split_inner_marks("קכא:ג)"), "קכא ג)");
        assert_eq!(split_inner_marks("לו:"), "לו:");
        assert_eq!(split_inner_marks("כו.)"), "כו.)");
        assert_eq!(split_inner_marks("כג:ב"), "כג ב");
    }
}
