//! Full-text search over Otzaria's Tantivy index, via the reused (vendored)
//! `search_engine` crate — reproducing the app's exact search semantics.
//!
//! Milestone 1 established the canonical mapping (verified against the live index):
//!   * `file_path` = `id:<bookId>` for official text books (`uid:` = user book,
//!     `ext:` = external); for PDFs it is the absolute file path and `is_pdf` is true.
//!   * `segment` = the seforim.db `line.lineIndex` for text books (or 0-based page for PDFs).
//!
//! So every text hit maps unambiguously to (`book_id`, `line_index`), which bridge
//! directly to get_text / get_links / get_book_info.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use search_engine::api::search_engine::{
    check_index_compatibility, ResultsOrder, SearchEngine, SearchResult,
};

pub struct Search {
    // SearchEngine holds an IndexWriter (Send, !Sync); a Mutex makes the whole
    // thing Sync so it can live behind an Arc and be used from blocking tasks.
    engine: Mutex<SearchEngine>,
}

#[derive(Debug, Clone, Copy)]
pub enum Mode {
    Exact,
    Advanced,
    Fuzzy,
}

impl Mode {
    pub fn parse(s: &str) -> Result<Mode> {
        match s {
            "exact" => Ok(Mode::Exact),
            "advanced" => Ok(Mode::Advanced),
            "fuzzy" => Ok(Mode::Fuzzy),
            other => bail!("unknown search mode '{other}' (expected: exact | advanced | fuzzy)"),
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Mode::Exact => "exact",
            Mode::Advanced => "advanced",
            Mode::Fuzzy => "fuzzy",
        }
    }
}

fn parse_order(s: &str) -> Result<ResultsOrder> {
    match s {
        "catalogue" => Ok(ResultsOrder::Catalogue),
        "relevance" => Ok(ResultsOrder::Relevance),
        other => bail!("unknown order '{other}' (expected: catalogue | relevance)"),
    }
}

impl Search {
    /// Open the Tantivy index (read-only use), optionally loading `lexical.db` for
    /// fuzzy morphological expansion. Fails cleanly on a schema-incompatible index
    /// instead of letting `SearchEngine::new` panic.
    pub fn open(
        index_dir: &std::path::Path,
        lexical_db: Option<&std::path::Path>,
    ) -> Result<Search> {
        let index_str = index_dir
            .to_str()
            .context("index path is not valid UTF-8")?
            .to_string();

        let compat = check_index_compatibility(index_str.clone());
        if !compat.compatible {
            bail!(
                "Tantivy index at {} is not compatible with the bundled engine (status={}, reason={:?}). \
                 The MCP's search_engine version must match the version that built the index.",
                index_dir.display(),
                compat.status,
                compat.reason
            );
        }

        let mut engine = SearchEngine::new(&index_str);
        if let Some(lex) = lexical_db {
            if let Some(lex_str) = lex.to_str() {
                // Best-effort: only affects fuzzy morphology; false just means "no lexicon".
                let loaded = engine.set_magic_dictionary_path(lex_str.to_string());
                if !loaded {
                    eprintln!(
                        "otzaria-mcp: lexical.db present but not loaded ({})",
                        lex.display()
                    );
                }
            }
        }

        Ok(Search {
            engine: Mutex::new(engine),
        })
    }

    pub fn doc_count(&self) -> u64 {
        self.engine.lock().unwrap().get_document_count()
    }

    pub fn search(
        &self,
        query: String,
        mode: Mode,
        limit: u32,
        offset: u32,
        facet: Option<String>,
        order: &str,
    ) -> Result<SearchResponse> {
        let order = parse_order(order)?;
        let facets: Vec<String> = facet.into_iter().collect();
        let engine = self.engine.lock().unwrap();

        let page = match mode {
            Mode::Exact => engine.search_and_count_exact(query, facets, limit, offset, order),
            // Advanced default distance 2 with no per-word options — mirrors the app's
            // default advanced mode. custom_spacing / alternative_words / search_options
            // are the app's advanced-dialog knobs; empty = plain morphological search.
            Mode::Advanced => engine.search_and_count_advanced(
                query,
                facets,
                limit,
                offset,
                2,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                order,
            ),
            Mode::Fuzzy => engine.search_and_count_fuzzy(query, facets, limit, offset, 2, order),
        }
        .map_err(|e| anyhow::anyhow!("tantivy search failed: {e}"))?;

        let hits = page
            .results
            .into_iter()
            .map(SearchHit::from_result)
            .collect();
        Ok(SearchResponse {
            total: page.total_count,
            mode: mode.as_str(),
            returned: 0, // filled below
            hits,
        }
        .with_returned_count())
    }
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    /// Total matching documents in the index (not just this page).
    pub total: u32,
    pub mode: &'static str,
    pub returned: usize,
    pub hits: Vec<SearchHit>,
}

impl SearchResponse {
    fn with_returned_count(mut self) -> Self {
        self.returned = self.hits.len();
        self
    }
}

#[derive(Debug, Serialize)]
pub struct SearchHit {
    /// Canonical seforim.db book id for official text books (None for PDFs / external).
    pub book_id: Option<i64>,
    /// seforim.db line index for text books (== segment); None for PDFs.
    pub line_index: Option<i64>,
    /// For PDF hits: 1-based page number (segment + 1).
    pub page: Option<i64>,
    pub title: String,
    pub reference: String,
    /// Matched text with «…» around the matched term(s), HTML stripped.
    pub snippet: String,
    pub is_pdf: bool,
    pub file_path: String,
}

impl SearchHit {
    fn from_result(r: SearchResult) -> SearchHit {
        let (book_id, line_index, page) = if r.is_pdf {
            (None, None, Some(r.segment as i64 + 1))
        } else {
            (parse_book_id(&r.file_path), Some(r.segment as i64), None)
        };
        SearchHit {
            book_id,
            line_index,
            page,
            title: r.title,
            reference: r.reference,
            snippet: clean_snippet(&r.text),
            is_pdf: r.is_pdf,
            file_path: r.file_path,
        }
    }
}

/// Parse the seforim.db book id out of the index `file_path`.
/// Text books are indexed as `id:<bookId>` (official) or `uid:<bookId>` (user book).
fn parse_book_id(file_path: &str) -> Option<i64> {
    for prefix in ["id:", "uid:"] {
        if let Some(rest) = file_path.strip_prefix(prefix) {
            return rest.parse::<i64>().ok();
        }
    }
    None
}

/// Convert the engine's `<font color=red>…</font>` highlight markup into `«…»`
/// markers, then strip any remaining HTML so the snippet is plain, readable text.
fn clean_snippet(text: &str) -> String {
    let marked = text
        .replace("<font color=red>", "«")
        .replace("</font>", "»");
    crate::db::strip_html_tags(&marked)
}

#[cfg(test)]
mod tests {
    use super::{parse_book_id, Mode};

    #[test]
    fn parses_book_id_prefixes() {
        assert_eq!(parse_book_id("id:1"), Some(1));
        assert_eq!(parse_book_id("uid:57"), Some(57));
        assert_eq!(parse_book_id("/abs/path/book.pdf"), None);
        assert_eq!(parse_book_id("ext:foo"), None);
    }

    #[test]
    fn mode_parse_roundtrip() {
        assert!(matches!(Mode::parse("exact").unwrap(), Mode::Exact));
        assert!(matches!(Mode::parse("advanced").unwrap(), Mode::Advanced));
        assert!(matches!(Mode::parse("fuzzy").unwrap(), Mode::Fuzzy));
        assert!(Mode::parse("nope").is_err());
    }
}
