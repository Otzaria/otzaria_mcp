//! Line-text decoding and plain-text helpers.
//!
//! Since seforim.db schema 6 the text of a line lives in `line_content.content`
//! (and alternate editions in `version_line.content`), and on the published library
//! every non-NULL value is a **zstd frame compressed with a dictionary** stored in the
//! `zstd_dict` table (the frame header names the dictionary id). Older databases keep
//! plain TEXT in `line.content`. [`LineCodec`] decodes both, mirroring the app's
//! `LineContentCodec` and the search engine's own line source.

use std::collections::HashMap;

use anyhow::{Context, Result};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension};

/// The largest line in the library is ~1.3 MB; a frame that declares more is corrupt.
const MAX_LINE_BYTES: u64 = 16 * 1024 * 1024;

/// Decoder for `line_content` / `version_line` values.
pub struct LineCodec {
    zstd: Option<ZstdCodec>,
}

struct ZstdCodec {
    dicts: HashMap<u32, zstd::zstd_safe::DDict<'static>>,
    dctx: zstd::zstd_safe::DCtx<'static>,
}

impl LineCodec {
    /// A codec for a database without `zstd_dict` (plain TEXT rows only).
    pub fn plain() -> LineCodec {
        LineCodec { zstd: None }
    }

    /// Load every dictionary of `zstd_dict`, when the table exists.
    pub fn load(conn: &Connection) -> Result<LineCodec> {
        let has_dicts = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'zstd_dict'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !has_dicts {
            return Ok(LineCodec::plain());
        }
        let mut dicts = HashMap::new();
        let mut stmt = conn.prepare("SELECT dict FROM zstd_dict")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let bytes: Vec<u8> = row.get(0).context("zstd_dict.dict is not a BLOB")?;
            let id = zstd::zstd_safe::get_dict_id_from_dict(&bytes).map_or(0, |id| id.get());
            let ddict = zstd::zstd_safe::DDict::try_create(&bytes)
                .context("ZSTD_createDDict failed for a zstd_dict row")?;
            dicts.insert(id, ddict);
        }
        let dctx = zstd::zstd_safe::DCtx::try_create().context("ZSTD_createDCtx failed")?;
        Ok(LineCodec {
            zstd: Some(ZstdCodec { dicts, dctx }),
        })
    }

    /// Whether the database stores compressed rows.
    pub fn is_compressed(&self) -> bool {
        self.zstd.is_some()
    }

    /// The text of a content column value. NULL reads as an empty line (like the app).
    pub fn text(&mut self, value: &Value) -> Result<String> {
        match value {
            Value::Null => Ok(String::new()),
            Value::Text(s) => Ok(s.clone()),
            Value::Blob(frame) => {
                let codec = self
                    .zstd
                    .as_mut()
                    .context("a compressed (BLOB) line in a database without zstd_dict")?;
                let bytes = codec.decode(frame)?;
                Ok(String::from_utf8_lossy(&bytes).into_owned())
            }
            Value::Integer(i) => Ok(i.to_string()),
            Value::Real(r) => Ok(r.to_string()),
        }
    }

    /// Like [`Self::text`], but never fails: an undecodable row becomes a marker.
    pub fn text_lossy(&mut self, value: &Value) -> String {
        self.text(value)
            .unwrap_or_else(|e| format!("[undecodable line: {e:#}]"))
    }
}

impl ZstdCodec {
    fn decode(&mut self, frame: &[u8]) -> Result<Vec<u8>> {
        let size = match zstd::zstd_safe::get_frame_content_size(frame) {
            Ok(Some(size)) if size <= MAX_LINE_BYTES => size as usize,
            other => anyhow::bail!("invalid zstd frame (content size {other:?})"),
        };
        // SAFETY: reads at most `frame.len()` bytes of a valid slice.
        let dict_id = unsafe {
            zstd::zstd_safe::zstd_sys::ZSTD_getDictID_fromFrame(frame.as_ptr().cast(), frame.len())
        };
        let ddict = self
            .dicts
            .get(&dict_id)
            .with_context(|| format!("dictionary {dict_id} is missing from zstd_dict"))?;
        let mut out: Vec<u8> = Vec::with_capacity(size);
        let written = self
            .dctx
            .decompress_using_ddict(&mut out, frame, ddict)
            .map_err(|code| {
                anyhow::anyhow!(
                    "zstd decode failed: {}",
                    zstd::zstd_safe::get_error_name(code)
                )
            })?;
        if written != size {
            anyhow::bail!("zstd frame declared {size} bytes and produced {written}");
        }
        Ok(out)
    }
}

/// Decode one HTML entity body (the text between `&` and `;`).
fn decode_entity(body: &str) -> Option<String> {
    let named = match body {
        "nbsp" | "thinsp" | "ensp" | "emsp" => Some(" "),
        "lt" => Some("<"),
        "gt" => Some(">"),
        "quot" => Some("\""),
        "apos" => Some("'"),
        "amp" => Some("&"),
        "shy" | "zwnj" | "zwj" | "lrm" | "rlm" => Some(""),
        _ => None,
    };
    if let Some(s) = named {
        return Some(s.to_string());
    }
    let num = body.strip_prefix('#')?;
    let code = if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        num.parse::<u32>().ok()?
    };
    char::from_u32(code).map(|c| c.to_string())
}

/// The visible units of an HTML line, in the coordinate system of `line.charCount` and
/// `link_anchor` offsets (SeforimLibrary's `countVisibleChars`): tags are skipped, an
/// entity `&…;` (terminated within 10 chars) is ONE unit, and every other char counts
/// its UTF-16 length (an astral char is followed by an empty unit).
pub fn visible_units(html: &str) -> Vec<String> {
    let chars: Vec<char> = html.chars().collect();
    let mut units = Vec::with_capacity(chars.len());
    let mut in_tag = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_tag {
            if c == '>' {
                in_tag = false;
            }
        } else if c == '<' {
            in_tag = true;
        } else if c == '&' {
            // Kotlin scans up to 10 UTF-16 units; Hebrew/ASCII entities are BMP.
            let end = (i + 10).min(chars.len());
            let semi = (i + 1..end).find(|&j| chars[j] == ';');
            match semi {
                Some(j) => {
                    let body: String = chars[i + 1..j].iter().collect();
                    units.push(decode_entity(&body).unwrap_or_else(|| format!("&{body};")));
                    i = j;
                }
                None => units.push("&".to_string()),
            }
        } else {
            units.push(c.to_string());
            if c.len_utf16() == 2 {
                units.push(String::new());
            }
        }
        i += 1;
    }
    units
}

/// Strip HTML tags and decode entities. `line.content` is raw HTML (`<h1>`, `<b>`,
/// footnote markup, …); most consumers want plain text.
pub fn strip_html_tags(input: &str) -> String {
    strip_html_tags_limited(input, None)
}

/// Stream visible text into one output buffer. With a limit, stop as soon as the
/// trimmed text is known to exceed it, which keeps snippets from scanning the rest
/// of very long lines.
fn strip_html_tags_limited(input: &str, limit: Option<usize>) -> String {
    let capacity = limit.map_or(input.len(), |max| {
        input
            .len()
            .min(max.saturating_mul(4).saturating_add('…'.len_utf8()))
    });
    let mut out = String::with_capacity(capacity);
    let mut cursor = 0;
    let mut started = false;
    let mut visible_chars = 0;

    while cursor < input.len() {
        let Some(relative) = input[cursor..].find(['<', '&']) else {
            if append_visible_text(
                &mut out,
                &input[cursor..],
                limit,
                &mut started,
                &mut visible_chars,
            ) {
                return out;
            }
            break;
        };
        let special = cursor + relative;
        if append_visible_text(
            &mut out,
            &input[cursor..special],
            limit,
            &mut started,
            &mut visible_chars,
        ) {
            return out;
        }

        if input.as_bytes()[special] == b'<' {
            cursor = input[special + 1..]
                .find('>')
                .map_or(input.len(), |end| special + 1 + end + 1);
            continue;
        }

        // Match visible_units' ten-unit search window (the ampersand plus at most
        // nine following Unicode scalar values) without materializing all chars.
        let body_start = special + 1;
        let semi = input[body_start..]
            .char_indices()
            .take(9)
            .find_map(|(offset, c)| (c == ';').then_some(body_start + offset));
        if let Some(semi) = semi {
            let body = &input[body_start..semi];
            if let Some(decoded) = decode_entity(body) {
                if append_visible_text(&mut out, &decoded, limit, &mut started, &mut visible_chars)
                {
                    return out;
                }
            } else {
                if append_visible_text(
                    &mut out,
                    &input[special..=semi],
                    limit,
                    &mut started,
                    &mut visible_chars,
                ) {
                    return out;
                }
            }
            cursor = semi + 1;
        } else {
            if append_visible_text(&mut out, "&", limit, &mut started, &mut visible_chars) {
                return out;
            }
            cursor = body_start;
        }
    }

    out.trim_end().to_string()
}

/// Append text after applying the same Unicode whitespace trim as `str::trim`, and
/// return true once a non-whitespace character proves the limited result is cut.
fn append_visible_text(
    out: &mut String,
    text: &str,
    limit: Option<usize>,
    started: &mut bool,
    visible_chars: &mut usize,
) -> bool {
    for c in text.chars() {
        if !*started && c.is_whitespace() {
            continue;
        }
        *started = true;
        *visible_chars += 1;
        if limit.is_some_and(|max| *visible_chars > max) {
            if !c.is_whitespace() {
                out.push('…');
                return true;
            }
        } else {
            out.push(c);
        }
    }
    false
}

/// Truncate to at most `n` chars (char-safe), appending `…` when cut.
pub fn truncate_text(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    }
}

/// Plain snippet of an HTML line: stripped and truncated.
pub fn snippet(html: &str, n: usize) -> String {
    strip_html_tags_limited(html, Some(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tags_and_decodes_entities() {
        assert_eq!(strip_html_tags("<b>שלום</b>"), "שלום");
        assert_eq!(strip_html_tags("א&#x27;ב"), "א'ב");
        assert_eq!(strip_html_tags("<h1>פרק</h1> א&nbsp;ב"), "פרק א ב");
        assert_eq!(strip_html_tags("&amp;"), "&");
        assert_eq!(strip_html_tags("a & b"), "a & b");
    }

    #[test]
    fn snippets_stream_trimmed_text_and_keep_truncate_semantics() {
        assert_eq!(snippet("<b>  אב ג ד</b>", 3), "אב …");
        assert_eq!(snippet("<b>אב&nbsp; </b>", 3), "אב");
        assert_eq!(snippet("<b>אב&nbsp; </b>", 2), "אב");
    }

    #[test]
    fn visible_units_match_the_library_char_counter() {
        // tags skipped, an entity is one unit
        let units = visible_units("<b>אב</b>&nbsp;ג");
        assert_eq!(units.len(), 4);
        assert_eq!(units.concat(), "אב ג");
        // an unterminated ampersand counts as one char
        assert_eq!(visible_units("a&b").len(), 3);
    }

    #[test]
    fn plain_codec_reads_text_and_null() {
        let mut codec = LineCodec::plain();
        assert_eq!(codec.text(&Value::Text("x".into())).unwrap(), "x");
        assert_eq!(codec.text(&Value::Null).unwrap(), "");
        assert!(codec.text(&Value::Blob(vec![1, 2, 3])).is_err());
    }

    #[test]
    fn zstd_dictionary_frames_round_trip() {
        // Build a tiny dictionary-compressed frame the way the library builder does.
        let samples: Vec<Vec<u8>> = (0..200)
            .map(|i| format!("בראשית ברא אלהים את השמים ואת הארץ {i}").into_bytes())
            .collect();
        let dict = zstd::dict::from_samples(&samples, 4096).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE zstd_dict (id INTEGER PRIMARY KEY, dict BLOB NOT NULL);")
            .unwrap();
        conn.execute("INSERT INTO zstd_dict (id, dict) VALUES (1, ?1)", [&dict])
            .unwrap();
        let text = "בראשית ברא אלהים";
        let mut compressor = zstd::bulk::Compressor::with_dictionary(3, &dict).unwrap();
        let frame = compressor.compress(text.as_bytes()).unwrap();

        let mut codec = LineCodec::load(&conn).unwrap();
        assert!(codec.is_compressed());
        assert_eq!(codec.text(&Value::Blob(frame)).unwrap(), text);
    }
}
