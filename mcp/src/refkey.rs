//! Canonical line-reference keys — based on the app's `lib/utils/text/ref_key.dart`
//! (itself kept identical to SeforimLibrary's builder, which fills `line_ref`).
//! Quoted ע"א/ע"ב are treated as amud marks only after a Hebrew numeral; elsewhere
//! they remain the numbers 71/72. Published v32 heRefs use unquoted chapter numbers.
//!
//! seforim.db (schema 6) has a `line_ref(bookId, refKeyHash, lineIndex)` index: for every
//! line, the FNV-1a-64 hash of the canonical key of its `heRef` *after the book title*
//! (e.g. `"ברכות, ב., א"` → key `"ב א א"`), plus "partial" keys (`"~ …"`) that omit the
//! named parts of a heRef. Find-ref computes the same key from the typed address and looks
//! it up — so any drift from the app's normalisation is a silent miss. The golden values
//! in the tests below were verified against the real v32 `line_ref` table.

/// Locator words that are not part of a reference's values ("פרק לג פסוק ה" ↔ "לג ה").
const LOCATOR_WORDS: &[&str] = &[
    "פרק", "פסוק", "פסקה", "סעיף", "סימן", "הלכה", "משנה", "מאמר", "דף", "עמוד", "אות",
];

/// Marker of a partial key; normalised tokens never contain it.
pub const PARTIAL_REF_KEY_MARKER: &str = "~";

fn is_heb_letter(c: char) -> bool {
    ('\u{05D0}'..='\u{05EA}').contains(&c)
}

fn is_quote(c: char) -> bool {
    matches!(c, '"' | '\'' | '\u{05F4}' | '\u{05F3}')
}

/// "ב." → "ב א", "ב:" → "ב ב" for a run of 1–3 letters not preceded by a letter or a
/// quote mark, when the mark is followed by `,`, whitespace or the end (Dart
/// `_expandDafMarks`).
fn expand_daf_marks(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 8);
    for (i, &c) in chars.iter().enumerate() {
        if c == '.' || c == ':' {
            let followed_ok = match chars.get(i + 1) {
                None => true,
                Some(&n) => n == ',' || n.is_whitespace(),
            };
            let mut run = 0;
            while run < i && is_heb_letter(chars[i - 1 - run]) {
                run += 1;
            }
            let preceded_ok = run == i || {
                let p = chars[i - 1 - run];
                !is_heb_letter(p) && !is_quote(p)
            };
            if followed_ok && (1..=3).contains(&run) && preceded_ok {
                out.push_str(if c == '.' { " א" } else { " ב" });
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Drop nikud/te'amim (U+0591–U+05C7); maqaf, paseq and `|` become spaces (Dart
/// `removeVolwels` + `removeTeamim`).
fn remove_marks(s: &str) -> String {
    s.chars()
        .filter_map(|c| match c {
            '\u{05BE}' | '\u{05C0}' | '|' => Some(' '),
            '\u{0591}'..='\u{05C7}' => None,
            _ => Some(c),
        })
        .collect()
}

/// `ע"א` / `ע"ב` (with any quote mark) → `א` / `ב` when preceded by a Hebrew
/// numeral. Without that context the sequence is a Hebrew number (71/72), as in
/// `תהילים ע"א`.
fn map_amud_marks(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == 'ע'
            && i + 2 < chars.len()
            && is_quote(chars[i + 1])
            && matches!(chars[i + 2], 'א' | 'ב')
            && (i == 0 || !is_heb_letter(chars[i - 1]))
            && chars.get(i + 3).is_none_or(|&n| !is_heb_letter(n))
            && previous_token_is_hebrew_numeral(&chars, i)
        {
            out.push(chars[i + 2]);
            i += 3;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn previous_token_is_hebrew_numeral(chars: &[char], before: usize) -> bool {
    let mut end = before;
    while end > 0 && !is_heb_letter(chars[end - 1]) {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && is_heb_letter(chars[start - 1]) {
        start -= 1;
    }
    if start == end {
        return false;
    }
    is_hebrew_numeral(&chars[start..end].iter().collect::<String>())
}

/// One numeric component of a trailing range: digits, or 1–3 letters optionally
/// followed by a quote mark and 1–2 letters; then an optional `.`/`:`. Returns the
/// length consumed from `start`.
fn range_component(chars: &[char], start: usize) -> Option<usize> {
    let mut i = start;
    if chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
        while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
    } else {
        let mut n = 0;
        while n < 3 && chars.get(i).is_some_and(|&c| is_heb_letter(c)) {
            i += 1;
            n += 1;
        }
        if n == 0 {
            return None;
        }
        if chars.get(i).is_some_and(|&c| is_quote(c)) {
            let mut m = 0;
            let mut j = i + 1;
            while m < 2 && chars.get(j).is_some_and(|&c| is_heb_letter(c)) {
                j += 1;
                m += 1;
            }
            if m > 0 {
                i = j;
            }
        }
    }
    if chars.get(i).is_some_and(|&c| c == '.' || c == ':') {
        i += 1;
    }
    Some(i - start)
}

/// Cut a trailing range ("לב יא-יג", "ברכות ב.-ג.") at its dash (Dart `_trailingRange`).
/// Only that shape is cut: a dash inside a book name ("מגדל־עז …") is kept.
fn cut_trailing_range(s: &str) -> &str {
    let chars: Vec<char> = s.chars().collect();
    let is_dash = |c: char| matches!(c, '-' | '\u{2013}' | '\u{05BE}');
    for (d, &c) in chars.iter().enumerate() {
        if !is_dash(c) {
            continue;
        }
        // Right side: \s* component \s* $
        let mut r = d + 1;
        while chars.get(r).is_some_and(|c| c.is_whitespace()) {
            r += 1;
        }
        let Some(len) = range_component(&chars, r) else {
            continue;
        };
        let mut e = r + len;
        while chars.get(e).is_some_and(|c| c.is_whitespace()) {
            e += 1;
        }
        if e != chars.len() {
            continue;
        }
        // Left side: (^|[\s,.:]) component \s* before the dash.
        let mut l = d;
        while l > 0 && chars[l - 1].is_whitespace() {
            l -= 1;
        }
        let left_ok = (0..l).any(|start| {
            let boundary = start == 0
                || matches!(chars[start - 1], ',' | '.' | ':')
                || chars[start - 1].is_whitespace();
            boundary && range_component(&chars, start) == Some(l - start)
        });
        if left_ok && d > 0 {
            let byte = s.char_indices().nth(d).map(|(b, _)| b).unwrap_or(s.len());
            return &s[..byte];
        }
    }
    s
}

/// Tokens of the canonical key of `reference` (Dart `refKeyTokens`): cut a trailing
/// range, expand daf marks, drop nikud/te'amim, map contextual ע"א/ע"ב amud marks,
/// drop quote marks and punctuation, lowercase, drop locator words.
pub fn ref_key_tokens(reference: &str) -> Vec<String> {
    let head = cut_trailing_range(reference);
    let cleaned = map_amud_marks(&remove_marks(&expand_daf_marks(head)));
    let cleaned: String = cleaned
        .chars()
        .filter(|&c| !is_quote(c))
        .map(|c| {
            if c.is_ascii_alphanumeric()
                || ('\u{0590}'..='\u{05FF}').contains(&c)
                || c.is_whitespace()
            {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .to_lowercase();
    cleaned
        .split_whitespace()
        .filter(|t| !LOCATOR_WORDS.contains(t))
        .map(str::to_string)
        .collect()
}

/// The canonical key of `reference`, or `None` when nothing is left of it.
pub fn build_ref_key(reference: &str) -> Option<String> {
    let tokens = ref_key_tokens(reference);
    (!tokens.is_empty()).then(|| tokens.join(" "))
}

/// The partial key of a typed address that may omit the named parts of a heRef.
pub fn build_partial_ref_key(reference: &str) -> Option<String> {
    build_ref_key(reference).map(|k| format!("{PARTIAL_REF_KEY_MARKER} {k}"))
}

/// Whether `token` is a Hebrew numeral of 1–3 letters (Dart `isHebrewNumeral`).
pub fn is_hebrew_numeral(token: &str) -> bool {
    let chars: Vec<char> = token.chars().collect();
    if chars.is_empty() || chars.len() > 3 {
        return false;
    }
    if token == "טו" || token == "טז" {
        return true;
    }
    let mut place = 3;
    for c in chars {
        let p = match c {
            'א'..='ט' => 1,
            'י' | 'כ' | 'ל' | 'מ' | 'נ' | 'ס' | 'ע' | 'פ' | 'צ' => 2,
            'ק'..='ת' => 3,
            _ => return false,
        };
        if p > place || (p == place && p != 3) {
            return false;
        }
        place = p;
    }
    true
}

/// `ע"א`/`ע"ב` after a Hebrew numeral → `עמוד א`/`עמוד ב` ("ברכות ב ע"א"); after a word
/// ("תהלים ע"א") they are the numbers 71/72 and stay (Dart `expandQueryAmudMarks`).
pub fn expand_query_amud_marks(query: &str) -> String {
    let words: Vec<&str> = query.split(' ').collect();
    let mut out: Vec<String> = Vec::with_capacity(words.len());
    for (i, w) in words.iter().enumerate() {
        let chars: Vec<char> = w.chars().collect();
        let is_amud = chars.len() == 3
            && chars[0] == 'ע'
            && is_quote(chars[1])
            && matches!(chars[2], 'א' | 'ב');
        if is_amud && i > 0 {
            let prev: String = words[i - 1].chars().filter(|&c| !is_quote(c)).collect();
            if is_hebrew_numeral(&prev) {
                out.push(format!("עמוד {}", chars[2]));
                continue;
            }
        }
        out.push((*w).to_string());
    }
    out.join(" ")
}

/// FNV-1a 64-bit over the UTF-8 bytes of `key`, as a signed value — the form stored in
/// `line_ref.refKeyHash`.
pub fn ref_key_hash(key: &str) -> i64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_match_the_library_line_ref_table() {
        // Verified against line_ref in the v32 seforim.db.
        assert_eq!(ref_key_hash("ב א א"), -7407518444830233145);
        assert_eq!(ref_key_hash("א א"), 2075999474238368921);
        assert_eq!(ref_key_hash("קיט א"), -8001436894454047873);
    }

    #[test]
    fn keys_follow_the_app_normalisation() {
        assert_eq!(build_ref_key(", ב., א").as_deref(), Some("ב א א"));
        assert_eq!(build_ref_key("ב:").as_deref(), Some("ב ב"));
        assert_eq!(build_ref_key("פרק א פסוק ב").as_deref(), Some("א ב"));
        assert_eq!(build_ref_key("בְּרֵאשִׁית א, א").as_deref(), Some("בראשית א א"));
        assert_eq!(build_ref_key("ב ע\"א").as_deref(), Some("ב א"));
        assert_eq!(build_ref_key("ע\"א א").as_deref(), Some("עא א"));
        assert_eq!(build_ref_key("ע\"ב א").as_deref(), Some("עב א"));
        assert_eq!(build_ref_key("פרק ע\"א פסוק א").as_deref(), Some("עא א"));
        assert_eq!(build_ref_key("ע\"א-ע\"ב").as_deref(), Some("עא"));
        assert_eq!(build_ref_key("לב יא-יג").as_deref(), Some("לב יא"));
        assert_eq!(build_ref_key("ב.-ג.").as_deref(), Some("ב א"));
        assert_eq!(build_partial_ref_key("שט ג").as_deref(), Some("~ שט ג"));
        // a quoted abbreviation is not a daf mark
        assert_eq!(build_ref_key("פ\"א.").as_deref(), Some("פא"));
    }

    #[test]
    fn numerals_and_amud_query_marks() {
        assert!(is_hebrew_numeral("קכא"));
        assert!(is_hebrew_numeral("טו"));
        assert!(!is_hebrew_numeral("שבת"));
        assert_eq!(expand_query_amud_marks("ברכות ב ע\"א"), "ברכות ב עמוד א");
        assert_eq!(expand_query_amud_marks("תהלים ע\"א"), "תהלים ע\"א");
    }
}
