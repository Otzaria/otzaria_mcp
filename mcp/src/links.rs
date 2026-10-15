//! Links, commentaries, commentators and link anchors — the app's link model.
//!
//! A `link` row is stored in ONE canonical direction: text-dependent works (commentary,
//! targum, midrash, …) are stored base → dependant, so the base text is the *source*
//! (side 0) and the commentary the *target* (side 1). Mirroring
//! `database_library_provider.dart`:
//! * **outgoing** (the app's forward query): links whose source line is in the window,
//!   plus links whose *source range* covers a window line (`link_coverage`, side 0);
//!   links whose side 0 is suppressed (`link_suppressed_side`) are hidden.
//! * **incoming** (the inverse query): links whose target line is in the window (plus
//!   `link_coverage` side 1), restricted to the text-dependent and reference types; a
//!   text-dependent link seen from the dependant side is the virtual type `SOURCE`
//!   ("what this comments on"). Suppressed side 1 is hidden.
//! * `link_range` gives the far side's end line; `link_anchor` gives word-level anchors
//!   (visible-char offsets into the anchored line — see [`crate::text::visible_units`]).

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use serde::Serialize;

use crate::db::{Db, Era, Inner, NamedBook};
use crate::text;

/// Text-dependent types — shown in the commentaries panel (`LinkTypes.dependentTextTypes`).
pub const DEPENDENT_TYPES: &[&str] = &[
    "COMMENTARY",
    "SUPER_COMMENTARY",
    "TARGUM",
    "MIDRASH",
    "PARSHANUT",
    "DIBUR_HAMATCHIL",
    "ELUCIDATION",
    "EXPLICATION",
    "FOOTNOTES",
];

/// Lateral reference types — both sides are equivalent (`LinkTypes.referenceTypes`).
pub const REFERENCE_TYPES: &[&str] = &[
    "REFERENCE",
    "QUOTATION",
    "MESORAT_HASHAS",
    "EIN_MISHPAT",
    "MISHNAH_IN_TALMUD",
    "RELATED",
    "OTHER",
    "SIFREI_MITZVOT",
    "ESSAY",
    "ALLUSION",
    "LITURGY",
    "LAW",
    "SUMMARY",
];

/// Hebrew label of a connection type (Sefaria's wording, as the app shows it).
pub fn hebrew_label(t: &str) -> &'static str {
    match t {
        "COMMENTARY" => "פירוש",
        "SUPER_COMMENTARY" => "פירוש על פירוש",
        "TARGUM" => "תרגום",
        "MIDRASH" => "מדרש",
        "PARSHANUT" => "פרשנות",
        "DIBUR_HAMATCHIL" => "דיבור המתחיל",
        "ELUCIDATION" | "EXPLICATION" => "ביאור",
        "REFERENCE" => "עיון",
        "QUOTATION" => "ציטוט",
        "MESORAT_HASHAS" => "מסורת הש\"ס",
        "EIN_MISHPAT" => "עין משפט",
        "MISHNAH_IN_TALMUD" => "משנה בתלמוד",
        "RELATED" => "נושא קרוב",
        "SOURCE" => "מקור",
        "ALLUSION" => "אזכור",
        "LAW" => "פסיקת הלכה",
        "FOOTNOTES" => "הערות שוליים",
        "LITURGY" => "תפילה",
        "LINKER" => "אוטומטי",
        "SUMMARY" => "סיכום",
        "SIFREI_MITZVOT" => "ספרי מצוות",
        "ESSAY" => "מאמר",
        _ => "לא מסווג",
    }
}

fn quoted(types: &[&str]) -> String {
    types
        .iter()
        .map(|t| format!("'{t}'"))
        .collect::<Vec<_>>()
        .join(",")
}

/// One link as seen from the queried book.
#[derive(Debug, Serialize)]
pub struct LinkRow {
    pub link_id: i64,
    /// Stored connection type (COMMENTARY, TARGUM, REFERENCE, LINKER, …).
    pub connection_type: String,
    /// What the app shows: `SOURCE` for a text-dependent link seen from the dependant
    /// side, otherwise the stored type.
    pub display_type: String,
    pub display_label: &'static str,
    /// "outgoing" (the queried book is the stored source) or "incoming".
    pub direction: &'static str,
    /// The queried book's line this link is attached to (a covered line for range links).
    pub at_line_index: i64,
    /// The linked (other) side.
    pub book_id: i64,
    pub book_title: String,
    pub line_index: i64,
    /// Last line of the linked side when the link spans a range (`link_range`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_end_index: Option<i64>,
    pub he_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub he_ref_end: Option<String>,
    pub snippet: String,
    /// 0 = none, 1 = inferred from the title ("X on Y"), 2 = declared by Sefaria.
    pub base_provenance: i64,
    /// Word-level anchors in the queried line (visible-char offsets), when stored.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub anchors: Vec<AnchorSpan>,
    /// Anchors on the linked side.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub linked_anchors: Vec<AnchorSpan>,
}

#[derive(Debug, Serialize, Clone)]
pub struct AnchorSpan {
    pub char_start: i64,
    /// Exclusive end; absent for a point anchor (an inline marker).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub char_end: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Raw row of a window query before snippets/anchors are attached.
struct RawLink {
    link_id: i64,
    ctype: String,
    at_line_index: i64,
    anchor_line_id: i64,
    displayed_line_id: i64,
    book_id: i64,
    book_title: String,
    other_line_id: i64,
    line_index: i64,
    he_ref: Option<String>,
    end_index: Option<i64>,
    end_ref: Option<String>,
    provenance: i64,
}

/// Which links a window query returns.
pub struct LinkFilter<'a> {
    /// Stored connection-type names to keep (None = all).
    pub types: Option<&'a [String]>,
    /// Only these linked books.
    pub book_ids: Option<&'a [i64]>,
}

impl Db {
    /// Count unique outgoing links for pagination without loading snippets or anchors.
    fn count_outgoing_links(
        &self,
        inner: &mut Inner,
        book_id: i64,
        window: Option<(i64, i64)>,
        filter: &LinkFilter,
    ) -> Result<i64> {
        let mut params: Vec<rusqlite::types::Value> = Vec::new();
        let anchors_cte = match window {
            Some((start, end)) => {
                params.extend([book_id.into(), start.into(), end.into()]);
                let coverage = if self.caps.link_coverage {
                    " UNION SELECT lc.linkId FROM link_coverage lc JOIN win w ON w.id = lc.lineId WHERE lc.side = 0"
                } else {
                    ""
                };
                format!(
                    "WITH win(id) AS (SELECT id FROM line WHERE bookId = ?1 AND lineIndex BETWEEN ?2 AND ?3), \
                     anchors(linkId) AS (SELECT l.id FROM win w JOIN link l ON l.sourceLineId = w.id{coverage})"
                )
            }
            None => {
                params.push(book_id.into());
                "WITH anchors(linkId) AS (SELECT l.id FROM link l WHERE l.sourceBookId = ?1)"
                    .to_string()
            }
        };

        let mut wheres = Vec::new();
        if self.caps.link_suppressed_side {
            wheres.push(
                "NOT EXISTS (SELECT 1 FROM link_suppressed_side ss WHERE ss.linkId = l.id AND ss.side = 0)"
                    .to_string(),
            );
        }
        if let Some(types) = filter.types {
            if !types.is_empty() {
                let mut ors = Vec::new();
                for t in types {
                    params.push(t.clone().into());
                    ors.push(format!("?{}", params.len()));
                }
                wheres.push(format!("ct.name IN ({})", ors.join(",")));
            }
        }
        if let Some(ids) = filter.book_ids {
            if !ids.is_empty() {
                let mut ors = Vec::new();
                for id in ids {
                    params.push((*id).into());
                    ors.push(format!("?{}", params.len()));
                }
                wheres.push(format!("l.targetBookId IN ({})", ors.join(",")));
            }
        }
        let where_sql = if wheres.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", wheres.join(" AND "))
        };
        let sql = format!(
            "{anchors_cte} \
             SELECT COUNT(*) FROM anchors a JOIN link l ON l.id = a.linkId \
             LEFT JOIN connection_type ct ON ct.id = l.connectionTypeId {where_sql}"
        );
        Ok(inner
            .conn
            .query_row(&sql, rusqlite::params_from_iter(params.iter()), |r| {
                r.get(0)
            })?)
    }

    /// Range-window link query for one direction. `window` = None means the whole book.
    #[allow(clippy::too_many_arguments)]
    fn window_links(
        &self,
        inner: &mut Inner,
        book_id: i64,
        window: Option<(i64, i64)>,
        outgoing: bool,
        filter: &LinkFilter,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<RawLink>> {
        let caps = &self.caps;
        let (this_side, other_side) = if outgoing { (0, 1) } else { (1, 0) };
        let (this_line, other_line, other_book) = if outgoing {
            ("l.sourceLineId", "l.targetLineId", "l.targetBookId")
        } else {
            ("l.targetLineId", "l.sourceLineId", "l.sourceBookId")
        };
        let mut params: Vec<rusqlite::types::Value> = Vec::new();
        let anchors_cte = match window {
            Some((start, end)) => {
                params.extend([book_id.into(), start.into(), end.into()]);
                let coverage = if caps.link_coverage {
                    format!(
                        " UNION ALL SELECT lc.linkId, lc.lineId FROM link_coverage lc \
                           WHERE lc.side = {this_side} AND lc.lineId IN (SELECT id FROM win)"
                    )
                } else {
                    String::new()
                };
                format!(
                    "WITH win(id) AS (SELECT id FROM line WHERE bookId = ?1 AND lineIndex BETWEEN ?2 AND ?3), \
                     anchor_candidates(linkId, anchorLineId) AS ( \
                       SELECT l.id, {this_line} FROM win w CROSS JOIN link l ON {this_line} = w.id{coverage}), \
                     anchors(linkId, anchorLineId) AS ( \
                       SELECT linkId, anchorLineId FROM ( \
                         SELECT ac.linkId, ac.anchorLineId, \
                                ROW_NUMBER() OVER (PARTITION BY ac.linkId ORDER BY ln.lineIndex, ln.id) AS rn \
                           FROM anchor_candidates ac JOIN line ln ON ln.id = ac.anchorLineId \
                       ) WHERE rn = 1)"
                )
            }
            None => {
                params.push(book_id.into());
                let col = if outgoing {
                    "sourceBookId"
                } else {
                    "targetBookId"
                };
                format!(
                    "WITH anchors(linkId, anchorLineId) AS (SELECT l.id, {this_line} FROM link l WHERE l.{col} = ?1)"
                )
            }
        };

        let mut wheres = Vec::new();
        if caps.link_suppressed_side {
            wheres.push(format!(
                "NOT EXISTS (SELECT 1 FROM link_suppressed_side ss WHERE ss.linkId = l.id AND ss.side = {this_side})"
            ));
        }
        if !outgoing {
            // The app's inverse query: text-dependent + (with per-side verdicts) reference types.
            let mut types: Vec<&str> = DEPENDENT_TYPES.to_vec();
            if caps.link_suppressed_side {
                types.extend_from_slice(REFERENCE_TYPES);
            }
            // Beyond the app: a type asked for explicitly (e.g. LINKER, which the app
            // shows from the citing side only) is also returned from the cited side.
            let requested: Vec<String> = filter
                .types
                .unwrap_or_default()
                .iter()
                .filter(|t| t.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
                .cloned()
                .collect();
            types.extend(requested.iter().map(String::as_str));
            wheres.push(format!("ct.name IN ({})", quoted(&types)));
            wheres.push(format!(
                "(l.sourceBookId != l.targetBookId OR ct.name IN ({}))",
                quoted(REFERENCE_TYPES)
            ));
        }
        if let Some(types) = filter.types {
            if !types.is_empty() {
                let mut ors = Vec::new();
                for t in types {
                    params.push(t.clone().into());
                    ors.push(format!("?{}", params.len()));
                }
                // The virtual SOURCE type selects incoming text-dependent links.
                let source_virtual = if !outgoing && types.iter().any(|t| t == "SOURCE") {
                    format!(" OR ct.name IN ({})", quoted(DEPENDENT_TYPES))
                } else {
                    String::new()
                };
                wheres.push(format!("(ct.name IN ({}){source_virtual})", ors.join(",")));
            }
        }
        if let Some(ids) = filter.book_ids {
            if !ids.is_empty() {
                let mut ors = Vec::new();
                for id in ids {
                    params.push((*id).into());
                    ors.push(format!("?{}", params.len()));
                }
                wheres.push(format!("{other_book} IN ({})", ors.join(",")));
            }
        }
        let where_sql = if wheres.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", wheres.join(" AND "))
        };
        let (range_cols, range_join) = if caps.link_range {
            (
                "lr.endLineIndex, rl.heRef",
                format!(
                    "LEFT JOIN link_range lr ON lr.linkId = l.id AND lr.side = {other_side} \
                     LEFT JOIN line rl ON rl.id = lr.endLineId"
                ),
            )
        } else {
            ("NULL, NULL", String::new())
        };
        let provenance = if caps.link_base_provenance {
            "l.baseProvenance"
        } else {
            "0"
        };
        params.push(limit.into());
        let limit_ph = params.len();
        params.push(offset.into());
        let offset_ph = params.len();
        let sql = format!(
            "{anchors_cte} \
             SELECT l.id, COALESCE(ct.name, 'OTHER'), al.lineIndex, a.anchorLineId, {this_line}, \
                    ob.id, ob.title, ol.id, ol.lineIndex, ol.heRef, {range_cols}, {provenance} \
               FROM anchors a \
               JOIN link l ON l.id = a.linkId \
               JOIN line al ON al.id = a.anchorLineId \
               JOIN line ol ON ol.id = {other_line} \
               JOIN book ob ON ob.id = {other_book} \
               LEFT JOIN connection_type ct ON ct.id = l.connectionTypeId \
               {range_join} \
               {where_sql} \
              ORDER BY al.lineIndex, ob.orderIndex, ol.lineIndex, l.id \
              LIMIT ?{limit_ph} OFFSET ?{offset_ph}"
        );
        let mut stmt = inner.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params.iter()), |r| {
                Ok(RawLink {
                    link_id: r.get(0)?,
                    ctype: r.get(1)?,
                    at_line_index: r.get(2)?,
                    anchor_line_id: r.get(3)?,
                    displayed_line_id: r.get(4)?,
                    book_id: r.get(5)?,
                    book_title: r.get(6)?,
                    other_line_id: r.get(7)?,
                    line_index: r.get(8)?,
                    he_ref: r.get(9)?,
                    end_index: r.get(10)?,
                    end_ref: r.get(11)?,
                    provenance: r.get(12)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Anchors of the given links, by (linkId, side).
    fn anchors_of(
        &self,
        inner: &mut Inner,
        link_ids: &[i64],
    ) -> Result<HashMap<(i64, i64), Vec<AnchorSpan>>> {
        let mut out: HashMap<(i64, i64), Vec<AnchorSpan>> = HashMap::new();
        if !self.caps.link_anchor || link_ids.is_empty() {
            return Ok(out);
        }
        for chunk in link_ids.chunks(400) {
            let ph = vec!["?"; chunk.len()].join(",");
            let mut stmt = inner.conn.prepare(&format!(
                "SELECT linkId, side, charStart, charEnd, label FROM link_anchor \
                  WHERE linkId IN ({ph}) ORDER BY linkId, side, charStart"
            ))?;
            for row in stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    AnchorSpan {
                        char_start: r.get(2)?,
                        char_end: r.get(3)?,
                        label: r.get(4)?,
                    },
                ))
            })? {
                let (id, side, span) = row?;
                out.entry((id, side)).or_default().push(span);
            }
        }
        Ok(out)
    }

    /// Turn raw rows into [`LinkRow`]s: dedupe by link (a range link is listed once, at
    /// its first window line), decode the linked line's snippet, attach anchors.
    fn finish_links(
        &self,
        inner: &mut Inner,
        raw: Vec<RawLink>,
        outgoing: bool,
        snippet_chars: usize,
    ) -> Result<Vec<LinkRow>> {
        let mut seen = HashSet::new();
        let raw: Vec<RawLink> = raw.into_iter().filter(|r| seen.insert(r.link_id)).collect();
        let ids: Vec<i64> = raw.iter().map(|r| r.other_line_id).collect();
        let texts = self.texts_by_line_id(inner, &ids)?;
        let link_ids: Vec<i64> = raw.iter().map(|r| r.link_id).collect();
        let anchors = self.anchors_of(inner, &link_ids)?;
        let (this_side, other_side) = if outgoing { (0, 1) } else { (1, 0) };
        Ok(raw
            .into_iter()
            .map(|r| {
                let display_type = if !outgoing && DEPENDENT_TYPES.contains(&r.ctype.as_str()) {
                    "SOURCE".to_string()
                } else {
                    r.ctype.clone()
                };
                // Anchor offsets were computed against the anchored line itself — never
                // leak them onto covered lines of a range link.
                let own = if r.anchor_line_id == r.displayed_line_id {
                    anchors
                        .get(&(r.link_id, this_side))
                        .cloned()
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                LinkRow {
                    link_id: r.link_id,
                    display_label: hebrew_label(&display_type),
                    display_type,
                    connection_type: r.ctype,
                    direction: if outgoing { "outgoing" } else { "incoming" },
                    at_line_index: r.at_line_index,
                    book_id: r.book_id,
                    book_title: r.book_title,
                    line_index: r.line_index,
                    line_end_index: r.end_index,
                    he_ref: r.he_ref,
                    he_ref_end: r.end_ref,
                    snippet: texts
                        .get(&r.other_line_id)
                        .map(|t| text::snippet(t, snippet_chars))
                        .unwrap_or_default(),
                    base_provenance: r.provenance,
                    anchors: own,
                    linked_anchors: anchors
                        .get(&(r.link_id, other_side))
                        .cloned()
                        .unwrap_or_default(),
                }
            })
            .collect())
    }

    // ── get_links ────────────────────────────────────────────────────────────
    /// Links of a book, a line or a line range. `direction`: "outgoing" | "incoming" | "both".
    #[allow(clippy::too_many_arguments)]
    pub fn get_links(
        &self,
        book_id: i64,
        window: Option<(i64, i64)>,
        types: Option<&[String]>,
        linked_book_ids: Option<&[i64]>,
        direction: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<LinkRow>> {
        let want_out = matches!(direction, "outgoing" | "both");
        let want_in = matches!(direction, "incoming" | "both");
        if !want_out && !want_in {
            anyhow::bail!("direction must be one of: outgoing | incoming | both");
        }
        let filter = LinkFilter {
            types,
            book_ids: linked_book_ids,
        };
        let mut inner = self.lock();
        let mut rows = Vec::new();
        if want_out && want_in {
            let offset = offset.max(0);
            let mut remaining = limit.max(0);
            if remaining == 0 {
                return Ok(rows);
            }
            let outgoing_count = self.count_outgoing_links(&mut inner, book_id, window, &filter)?;
            let mut incoming_offset = 0;
            if offset < outgoing_count {
                let outgoing_take = remaining.min(outgoing_count - offset);
                let raw = self.window_links(
                    &mut inner,
                    book_id,
                    window,
                    true,
                    &filter,
                    outgoing_take,
                    offset,
                )?;
                rows.extend(self.finish_links(&mut inner, raw, true, 160)?);
                remaining -= outgoing_take;
            } else {
                incoming_offset = offset - outgoing_count;
            }
            if remaining > 0 {
                let raw = self.window_links(
                    &mut inner,
                    book_id,
                    window,
                    false,
                    &filter,
                    remaining,
                    incoming_offset,
                )?;
                rows.extend(self.finish_links(&mut inner, raw, false, 160)?);
            }
            return Ok(rows);
        }
        if want_out {
            let raw =
                self.window_links(&mut inner, book_id, window, true, &filter, limit, offset)?;
            rows.extend(self.finish_links(&mut inner, raw, true, 160)?);
        }
        if want_in {
            let raw =
                self.window_links(&mut inner, book_id, window, false, &filter, limit, offset)?;
            rows.extend(self.finish_links(&mut inner, raw, false, 160)?);
        }
        Ok(rows)
    }

    // ── get_commentaries ─────────────────────────────────────────────────────
    /// Commentaries / targumim / midrash on a line range: the commentary text itself
    /// (with its own range, when the comment spans lines) plus default commentators.
    pub fn get_commentaries(
        &self,
        book_id: i64,
        start_line: i64,
        end_line: i64,
        commentator_ids: Option<&[i64]>,
        limit: i64,
        max_lines_per_item: i64,
    ) -> Result<Commentaries> {
        let types: Vec<String> = DEPENDENT_TYPES.iter().map(|s| s.to_string()).collect();
        let filter = LinkFilter {
            types: Some(&types),
            book_ids: commentator_ids,
        };
        // Initialise the era cache BEFORE taking the connection lock (it locks too).
        let eras = self.eras();
        let mut inner = self.lock();
        let raw = self.window_links(
            &mut inner,
            book_id,
            Some((start_line, end_line)),
            true,
            &filter,
            limit,
            0,
        )?;
        let links = self.finish_links(&mut inner, raw, true, 0)?;
        let mut items = Vec::with_capacity(links.len());
        for l in links {
            let end = l
                .line_end_index
                .unwrap_or(l.line_index)
                .min(l.line_index + max_lines_per_item.max(1) - 1);
            let lines = self.lines_in_range(&mut inner, l.book_id, l.line_index, end)?;
            let text = lines
                .iter()
                .map(|(_, _, _, html)| text::strip_html_tags(html))
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            items.push(CommentaryItem {
                connection_type: l.connection_type,
                label: l.display_label,
                commentator_book_id: l.book_id,
                commentator_title: l.book_title,
                era: eras.get(&l.book_id).copied().unwrap_or(Era::Other),
                on_line_index: l.at_line_index,
                target_line_index: l.line_index,
                target_line_end_index: l.line_end_index,
                target_ref: l.he_ref,
                text,
                anchors: l.anchors,
            });
        }
        drop(inner);
        let (default_commentators, default_targumim) = self.defaults(book_id)?;
        Ok(Commentaries {
            default_commentators,
            default_targumim,
            items,
        })
    }

    fn defaults(&self, book_id: i64) -> Result<(Vec<NamedBook>, Vec<NamedBook>)> {
        let inner = self.lock();
        let named = |sql: &str| -> Result<Vec<NamedBook>> {
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
        let commentators = named(
            "SELECT b.id, b.title FROM default_commentator dc \
               JOIN book b ON dc.commentatorBookId = b.id WHERE dc.bookId = ?1 ORDER BY dc.position",
        )?;
        let targumim = if self.caps.default_targum {
            named(
                "SELECT b.id, b.title FROM default_targum dt \
                   JOIN book b ON dt.targumBookId = b.id WHERE dt.bookId = ?1 ORDER BY dt.position",
            )?
        } else {
            Vec::new()
        };
        Ok((commentators, targumim))
    }

    // ── list_commentators ────────────────────────────────────────────────────
    /// Which commentators (text-dependent works) are linked to a book or a line range,
    /// with link counts per type, grouped by era like the app's commentators panel.
    pub fn list_commentators(
        &self,
        book_id: i64,
        window: Option<(i64, i64)>,
    ) -> Result<CommentatorList> {
        let caps = &self.caps;
        let suppressed = if caps.link_suppressed_side {
            "AND NOT EXISTS (SELECT 1 FROM link_suppressed_side ss WHERE ss.linkId = l.id AND ss.side = 0)"
        } else {
            ""
        };
        let types = quoted(DEPENDENT_TYPES);
        let (source_sql, params): (String, Vec<i64>) = match window {
            Some((s, e)) => {
                let coverage = if caps.link_coverage {
                    " UNION SELECT lc.linkId FROM link_coverage lc WHERE lc.side = 0 AND lc.lineId IN (SELECT id FROM win)"
                } else {
                    ""
                };
                (
                    format!(
                        "WITH win(id) AS (SELECT id FROM line WHERE bookId = ?1 AND lineIndex BETWEEN ?2 AND ?3), \
                         ids(linkId) AS (SELECT l.id FROM win w CROSS JOIN link l ON l.sourceLineId = w.id{coverage}) \
                         SELECT l.targetBookId, ct.name, COUNT(*) FROM ids JOIN link l ON l.id = ids.linkId"
                    ),
                    vec![book_id, s, e],
                )
            }
            None => (
                "SELECT l.targetBookId, ct.name, COUNT(*) FROM link l".to_string(),
                vec![book_id],
            ),
        };
        let book_filter = if window.is_some() {
            ""
        } else {
            "AND l.sourceBookId = ?1"
        };
        let sql = format!(
            "{source_sql} JOIN connection_type ct ON ct.id = l.connectionTypeId \
              WHERE ct.name IN ({types}) {book_filter} {suppressed} \
              GROUP BY l.targetBookId, ct.name"
        );
        let counts: Vec<(i64, String, i64)> = {
            let inner = self.lock();
            let mut stmt = inner.conn.prepare(&sql)?;
            let v = stmt
                .query_map(rusqlite::params_from_iter(params.iter()), |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            v
        };
        let mut by_book: HashMap<i64, Vec<(String, i64)>> = HashMap::new();
        for (bid, t, n) in counts {
            by_book.entry(bid).or_default().push((t, n));
        }
        let ids: Vec<i64> = by_book.keys().copied().collect();
        let titles = self.book_titles(&ids)?;
        let (defaults, targumim) = self.defaults(book_id)?;
        let default_pos: HashMap<i64, usize> = defaults
            .iter()
            .chain(targumim.iter())
            .enumerate()
            .map(|(i, b)| (b.book_id, i))
            .collect();
        let orders: HashMap<i64, i64> = {
            let inner = self.lock();
            let mut m = HashMap::new();
            for chunk in ids.chunks(400) {
                let ph = vec!["?"; chunk.len()].join(",");
                let mut stmt = inner.conn.prepare(&format!(
                    "SELECT id, orderIndex FROM book WHERE id IN ({ph})"
                ))?;
                for row in stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
                })? {
                    let (id, o) = row?;
                    m.insert(id, o);
                }
            }
            m
        };

        let mut groups: HashMap<Era, Vec<Commentator>> = HashMap::new();
        let mut total = 0;
        for (bid, types) in by_book {
            let count: i64 = types.iter().map(|(_, n)| n).sum();
            total += count;
            let era = self.era_of(bid);
            groups.entry(era).or_default().push(Commentator {
                book_id: bid,
                title: titles.get(&bid).cloned().unwrap_or_default(),
                link_count: count,
                types: types.into_iter().collect(),
                default_position: default_pos.get(&bid).copied(),
            });
        }
        let mut eras: Vec<EraGroup> = groups
            .into_iter()
            .map(|(era, mut commentators)| {
                commentators.sort_by_key(|c| {
                    (
                        c.default_position.unwrap_or(usize::MAX),
                        orders.get(&c.book_id).copied().unwrap_or(i64::MAX),
                        c.book_id,
                    )
                });
                EraGroup {
                    era,
                    era_order: era.order(),
                    count: commentators.len(),
                    commentators,
                }
            })
            .collect();
        eras.sort_by_key(|g| g.era_order);
        Ok(CommentatorList {
            book_id,
            line_index: window.map(|w| w.0),
            end_line_index: window.map(|w| w.1),
            commentator_count: eras.iter().map(|g| g.count).sum(),
            link_count: total,
            default_commentators: defaults,
            default_targumim: targumim,
            eras,
        })
    }

    // ── get_line_anchors ─────────────────────────────────────────────────────
    /// Word-level link anchors inside one line: for every anchor, the anchored words
    /// (or, for a point anchor, the text around the marker) and the link's other side.
    pub fn get_line_anchors(&self, book_id: i64, line_index: i64) -> Result<Option<LineAnchors>> {
        let mut inner = self.lock();
        let lines = self.lines_in_range(&mut inner, book_id, line_index, line_index)?;
        let Some((_, line_id, he_ref, html)) = lines.into_iter().next() else {
            return Ok(None);
        };
        let units = text::visible_units(&html);
        let mut out = Vec::new();
        if self.caps.link_anchor {
            for (side, line_col, other_line, other_book) in [
                (0, "l.sourceLineId", "l.targetLineId", "l.targetBookId"),
                (1, "l.targetLineId", "l.sourceLineId", "l.sourceBookId"),
            ] {
                let suppressed = if self.caps.link_suppressed_side {
                    format!(
                        "AND NOT EXISTS (SELECT 1 FROM link_suppressed_side ss WHERE ss.linkId = l.id AND ss.side = {side})"
                    )
                } else {
                    String::new()
                };
                let sql = format!(
                    "SELECT la.charStart, la.charEnd, la.label, l.id, COALESCE(ct.name, 'OTHER'), \
                            ob.id, ob.title, ol.lineIndex, ol.heRef \
                       FROM link l \
                       JOIN link_anchor la ON la.linkId = l.id AND la.side = {side} \
                       JOIN line ol ON ol.id = {other_line} \
                       JOIN book ob ON ob.id = {other_book} \
                       LEFT JOIN connection_type ct ON ct.id = l.connectionTypeId \
                      WHERE {line_col} = ?1 {suppressed} \
                      ORDER BY la.charStart"
                );
                let mut stmt = inner.conn.prepare(&sql)?;
                let rows = stmt
                    .query_map([line_id], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, Option<i64>>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, String>(4)?,
                            r.get::<_, i64>(5)?,
                            r.get::<_, String>(6)?,
                            r.get::<_, i64>(7)?,
                            r.get::<_, Option<String>>(8)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for (start, end, label, link_id, ctype, ob, title, oli, oref) in rows {
                    let s = (start.max(0) as usize).min(units.len());
                    let (anchored_text, context) = match end {
                        Some(e) => {
                            let e = (e.max(0) as usize).clamp(s, units.len());
                            (Some(units[s..e].concat()), None)
                        }
                        None => {
                            let before = units[s.saturating_sub(24)..s].concat();
                            let after = units[s..(s + 24).min(units.len())].concat();
                            (
                                None,
                                Some(format!(
                                    "{before}⟨{}⟩{after}",
                                    label.as_deref().unwrap_or("•")
                                )),
                            )
                        }
                    };
                    let display_type = if side == 1 && DEPENDENT_TYPES.contains(&ctype.as_str()) {
                        "SOURCE".to_string()
                    } else {
                        ctype.clone()
                    };
                    out.push(LineAnchor {
                        char_start: start,
                        char_end: end,
                        label,
                        anchored_text,
                        context,
                        link_id,
                        connection_type: ctype,
                        display_label: hebrew_label(&display_type),
                        book_id: ob,
                        book_title: title,
                        line_index: oli,
                        he_ref: oref,
                    });
                }
            }
        }
        out.sort_by_key(|a| (a.char_start, a.link_id));
        Ok(Some(LineAnchors {
            book_id,
            line_index,
            he_ref,
            visible_length: units.len(),
            text: units.concat(),
            anchors: out,
        }))
    }
}

#[derive(Debug, Serialize)]
pub struct CommentaryItem {
    pub connection_type: String,
    pub label: &'static str,
    pub commentator_book_id: i64,
    pub commentator_title: String,
    pub era: Era,
    /// The base-text line this commentary is attached to.
    pub on_line_index: i64,
    pub target_line_index: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_line_end_index: Option<i64>,
    pub target_ref: Option<String>,
    pub text: String,
    /// Where in the base line the comment is anchored (visible-char offsets).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub anchors: Vec<AnchorSpan>,
}

#[derive(Debug, Serialize)]
pub struct Commentaries {
    pub default_commentators: Vec<NamedBook>,
    pub default_targumim: Vec<NamedBook>,
    pub items: Vec<CommentaryItem>,
}

#[derive(Debug, Serialize)]
pub struct Commentator {
    pub book_id: i64,
    pub title: String,
    pub link_count: i64,
    /// Link count per connection type.
    pub types: BTreeMap<String, i64>,
    /// Position in the book's default commentators/targumim (the app's default order).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_position: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct EraGroup {
    pub era: Era,
    pub era_order: u8,
    pub count: usize,
    pub commentators: Vec<Commentator>,
}

#[derive(Debug, Serialize)]
pub struct CommentatorList {
    pub book_id: i64,
    pub line_index: Option<i64>,
    pub end_line_index: Option<i64>,
    pub commentator_count: usize,
    pub link_count: i64,
    pub default_commentators: Vec<NamedBook>,
    pub default_targumim: Vec<NamedBook>,
    pub eras: Vec<EraGroup>,
}

#[derive(Debug, Serialize)]
pub struct LineAnchor {
    pub char_start: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub char_end: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The anchored words (range anchors).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchored_text: Option<String>,
    /// Text around a point anchor, with the marker shown as ⟨label⟩.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    pub link_id: i64,
    pub connection_type: String,
    pub display_label: &'static str,
    pub book_id: i64,
    pub book_title: String,
    pub line_index: i64,
    pub he_ref: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LineAnchors {
    pub book_id: i64,
    pub line_index: i64,
    pub he_ref: Option<String>,
    /// Length of `text` in anchor coordinates (visible chars).
    pub visible_length: usize,
    /// The line's visible text (HTML stripped) — anchor offsets index into it.
    pub text: String,
    pub anchors: Vec<LineAnchor>,
}
