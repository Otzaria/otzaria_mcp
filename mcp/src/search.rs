//! Full-text search over Otzaria's Tantivy index, via the reused (vendored)
//! `search_engine` 3.1.0 crate — reproducing the app's exact search semantics.
//!
//! Index contract (schema 5, verified against the app's indexer):
//!   * `file_path` = `id:<bookId>` for official text books (`uid:` = user book, `ext:` /
//!     `attached:` = external or attached catalogues); for PDFs a file path and `is_pdf`.
//!   * `segment` = the 0-based position of the line among the book's rows ordered by
//!     `lineIndex`, which is the seforim.db `line.lineIndex` (dense 0..n-1 in the library).
//!   * Official books' line text is NOT stored in the index (`TextStorage::LibraryDb`): the
//!     engine reads each result line back from seforim.db through its process-wide *library
//!     line source*, which must be configured with the DB path ([`configure_line_source`]).
//!     A result whose row no longer matches the indexed text is `stale`.
//!   * Facets: the category path (`/תנ״ך/תורה/…`) plus dimension facets `/author/<name>`,
//!     `/era/<era>` and `/base` (OR inside a dimension, AND across dimensions).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use search_engine::api::search_engine::{
    check_index_compatibility, line_source_status, refresh_line_source, split_query_words,
    FacetCount, ResultGrouping, ResultsOrder, SearchEngine, SearchPageResult, SearchResult,
    SearchScope, TextStatus, WordMatchMode,
};

pub struct Search {
    // SearchEngine is used from one thread at a time (its docs); a Mutex makes the whole
    // thing Sync so it can live behind an Arc and be used from blocking tasks.
    engine: Mutex<SearchEngine>,
    pub info: IndexInfo,
}

/// What was opened, for `library_info`.
#[derive(Debug, Clone, Serialize)]
pub struct IndexInfo {
    pub path: String,
    pub status: String,
    pub found_schema_version: Option<u32>,
    pub engine_schema_version: u32,
    pub engine_version: String,
    pub documents: u64,
    pub lexical_dictionary: bool,
    pub translation_dictionary: bool,
    pub acronyms_dictionary: bool,
    /// The seforim.db the engine reads official books' line text from.
    pub line_source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        "generation" => Ok(ResultsOrder::Generation),
        other => bail!("unknown order '{other}' (expected: catalogue | relevance | generation)"),
    }
}

fn parse_scope(s: &str) -> Result<SearchScope> {
    match s {
        "word_distance" => Ok(SearchScope::WordDistance),
        "same_paragraph" => Ok(SearchScope::SameParagraph),
        "same_section" => Ok(SearchScope::SameSection),
        other => bail!(
            "unknown scope '{other}' (expected: word_distance | same_paragraph | same_section)"
        ),
    }
}

fn parse_grouping(s: &str) -> Result<Option<ResultGrouping>> {
    match s {
        "none" | "" => Ok(None),
        "same_section" => Ok(Some(ResultGrouping::SameSection)),
        "identical_text" => Ok(Some(ResultGrouping::IdenticalText)),
        other => {
            bail!("unknown grouping '{other}' (expected: none | same_section | identical_text)")
        }
    }
}

/// `all` | `any` | `most` | `at_least:N`.
fn parse_word_match(s: &str) -> Result<(Option<WordMatchMode>, Option<u32>)> {
    match s {
        "all" | "" => Ok((None, None)),
        "any" => Ok((Some(WordMatchMode::AnyWord), None)),
        "most" => Ok((Some(WordMatchMode::MostWords), None)),
        other => match other.strip_prefix("at_least:").map(str::parse::<u32>) {
            Some(Ok(n)) => Ok((Some(WordMatchMode::AtLeast), Some(n))),
            _ => bail!("unknown word_match '{other}' (expected: all | any | most | at_least:N)"),
        },
    }
}

/// Per-word morphology options of the advanced mode (the app's advanced-search
/// checkboxes). They are applied to every word of the query.
#[derive(Debug, Clone, Default)]
pub struct WordOptions {
    pub prefixes: bool,
    pub suffixes: bool,
    pub grammatical_prefixes: bool,
    pub grammatical_suffixes: bool,
    pub full_deficient_spelling: bool,
    pub partial_word: bool,
    pub typos: bool,
    pub aramaic_prefixes: bool,
    pub aramaic_suffixes: bool,
    pub ignore_quotes: bool,
    pub acronyms: bool,
    pub aramaic_translation: bool,
}

impl WordOptions {
    /// The engine's UI keys (`hebrew_query::OPT_*`, which must match the Dart layer).
    fn keys(&self) -> Vec<&'static str> {
        let mut k = Vec::new();
        let mut add = |on: bool, key: &'static str| {
            if on {
                k.push(key)
            }
        };
        add(self.prefixes, "קידומות");
        add(self.suffixes, "סיומות");
        add(self.grammatical_prefixes, "קידומות דקדוקיות");
        add(self.grammatical_suffixes, "סיומות דקדוקיות");
        add(self.full_deficient_spelling, "כתיב מלא/חסר");
        add(self.partial_word, "חלק ממילה");
        add(self.typos, "שגיאות כתיב");
        add(self.aramaic_prefixes, "קידומות ארמיות");
        add(self.aramaic_suffixes, "סיומות ארמיות");
        add(self.ignore_quotes, "התעלם מגרשיים");
        add(self.acronyms, "ראשי תיבות");
        add(self.aramaic_translation, "תרגום ארמי");
        k
    }

    pub fn any(&self) -> bool {
        !self.keys().is_empty()
    }
}

/// The engine keys per-word options `"{word}_{index}"`, with the words split from the
/// query normalised like the index (marks stripped, lowercased).
fn per_word_options(query: &str, opts: &WordOptions) -> HashMap<String, HashMap<String, bool>> {
    let keys = opts.keys();
    let mut out = HashMap::new();
    if keys.is_empty() {
        return out;
    }
    let normalized: String = query
        .chars()
        .filter(|c| !matches!(c, '\u{0591}'..='\u{05BD}' | '\u{05BF}' | '\u{05C1}'..='\u{05C2}' | '\u{05C4}'..='\u{05C5}' | '\u{05C7}'))
        .collect::<String>()
        .to_lowercase();
    for (i, word) in split_query_words(normalized).into_iter().enumerate() {
        let map: HashMap<String, bool> = keys.iter().map(|k| (k.to_string(), true)).collect();
        out.insert(format!("{word}_{i}"), map);
    }
    out
}

/// A full-text query (all modes).
#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub mode: Mode,
    pub limit: u32,
    pub offset: u32,
    pub order: String,
    /// Engine facet paths (category paths and `/author/…`, `/era/…`, `/base`).
    pub facets: Vec<String>,
    /// Advanced/exact: max words between adjacent query words (app default 0).
    /// Fuzzy: max edit distance 0–2 (app default 2).
    pub distance: Option<u32>,
    pub options: WordOptions,
    pub negative_query: Option<String>,
    pub scope: String,
    pub word_match: String,
    pub match_nikud: bool,
    pub match_taamim: bool,
    pub grouping: String,
}

impl SearchRequest {
    pub fn new(query: impl Into<String>, mode: Mode) -> SearchRequest {
        SearchRequest {
            query: query.into(),
            mode,
            limit: 20,
            offset: 0,
            order: "catalogue".into(),
            facets: Vec::new(),
            distance: None,
            options: WordOptions::default(),
            negative_query: None,
            scope: "word_distance".into(),
            word_match: "all".into(),
            match_nikud: false,
            match_taamim: false,
            grouping: "none".into(),
        }
    }

    /// The advanced path is needed for anything only it supports.
    fn effective_mode(&self) -> Mode {
        if self.mode == Mode::Exact
            && (self.options.any()
                || self
                    .negative_query
                    .as_deref()
                    .is_some_and(|q| !q.trim().is_empty())
                || self.scope != "word_distance"
                || self.word_match != "all"
                || self.distance.is_some_and(|d| d > 0))
        {
            Mode::Advanced
        } else {
            self.mode
        }
    }
}

/// Everything the advanced engine calls take, parsed once.
struct Advanced {
    negative: String,
    distance: u32,
    options: HashMap<String, HashMap<String, bool>>,
    scope: SearchScope,
    word_match: (Option<WordMatchMode>, Option<u32>),
}

impl Advanced {
    fn from(req: &SearchRequest) -> Result<Advanced> {
        Ok(Advanced {
            negative: req.negative_query.clone().unwrap_or_default(),
            distance: req.distance.unwrap_or(0),
            options: per_word_options(&req.query, &req.options),
            scope: parse_scope(&req.scope)?,
            word_match: parse_word_match(&req.word_match)?,
        })
    }
}

fn fuzzy_distance(req: &SearchRequest) -> u8 {
    req.distance.unwrap_or(2).min(2) as u8
}

impl Search {
    /// Open the Tantivy index read-only. Fails cleanly on an incompatible index instead of
    /// letting the engine panic. Points the engine's library line source at `db_path`, and
    /// loads the optional lexicons: `lexical.db` (fuzzy morphology) and the advanced-mode
    /// dictionaries `dictionary.json` (Aramaic translation) / `Acronyms.json`.
    pub fn open(
        index_dir: &Path,
        lexical_db: Option<&Path>,
        db_path: &Path,
        dictionaries_dir: Option<&Path>,
    ) -> Result<Search> {
        let index_str = index_dir
            .to_str()
            .context("index path is not valid UTF-8")?
            .to_string();

        let compat = check_index_compatibility(index_str.clone());
        if !compat.compatible {
            bail!(
                "Tantivy index at {} is not compatible with the bundled engine {} \
                 (status={}, index schema {:?}, engine schema {}, reason={:?}). The MCP's \
                 search_engine version must match the version that built the index.",
                index_dir.display(),
                compat.engine_version,
                compat.status,
                compat.found_schema_version,
                compat.required_schema_version,
                compat.reason
            );
        }

        let db_str = db_path
            .to_str()
            .context("seforim.db path is not valid UTF-8")?
            .to_string();
        refresh_line_source(db_str.clone())
            .with_context(|| format!("refreshing the engine's line source at {db_str}"))?;

        let mut engine = SearchEngine::open_read_only(&index_str)?;
        let lexical = lexical_db
            .and_then(Path::to_str)
            .is_some_and(|p| engine.set_magic_dictionary_path(p.to_string()));
        let dict = |name: &str| -> Option<String> {
            let p: PathBuf = dictionaries_dir?.join(name);
            p.is_file().then(|| p.to_string_lossy().into_owned())
        };
        let translation =
            dict("dictionary.json").is_some_and(|p| engine.set_translation_dictionary_path(p));
        let acronyms =
            dict("Acronyms.json").is_some_and(|p| engine.set_acronyms_dictionary_path(p));

        let info = IndexInfo {
            path: index_str,
            status: compat.status,
            found_schema_version: compat.found_schema_version,
            engine_schema_version: compat.required_schema_version,
            engine_version: compat.engine_version,
            documents: engine.get_document_count(),
            lexical_dictionary: lexical,
            translation_dictionary: translation,
            acronyms_dictionary: acronyms,
            line_source: db_str,
        };
        Ok(Search {
            engine: Mutex::new(engine),
            info,
        })
    }

    pub fn doc_count(&self) -> u64 {
        self.info.documents
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SearchEngine> {
        self.engine.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn search(&self, req: &SearchRequest) -> Result<SearchResponse> {
        let order = parse_order(&req.order)?;
        let grouping = parse_grouping(&req.grouping)?;
        let mode = req.effective_mode();
        let engine = self.lock();
        let page: SearchPageResult = match mode {
            Mode::Exact => engine.search_and_count_exact(
                req.query.clone(),
                req.facets.clone(),
                req.limit,
                req.offset,
                order,
                req.match_nikud,
                req.match_taamim,
                grouping,
            ),
            Mode::Fuzzy => engine.search_and_count_fuzzy(
                req.query.clone(),
                req.facets.clone(),
                req.limit,
                req.offset,
                fuzzy_distance(req),
                order,
                req.match_nikud,
                req.match_taamim,
                grouping,
            ),
            Mode::Advanced => {
                let a = Advanced::from(req)?;
                engine.search_and_count_advanced(
                    req.query.clone(),
                    a.negative,
                    req.facets.clone(),
                    req.limit,
                    req.offset,
                    a.distance,
                    a.distance,
                    HashMap::new(),
                    HashMap::new(),
                    HashMap::new(),
                    HashMap::new(),
                    a.options,
                    HashMap::new(),
                    order,
                    req.match_nikud,
                    req.match_taamim,
                    a.scope,
                    SearchScope::WordDistance,
                    grouping,
                    a.word_match.0,
                    a.word_match.1,
                )
            }
        }
        .map_err(|e| anyhow::anyhow!("tantivy search failed: {e:#}"))?;

        let hits: Vec<SearchHit> = page
            .results
            .into_iter()
            .map(SearchHit::from_result)
            .collect();
        Ok(SearchResponse {
            total: page.total_count,
            group_count: page.group_count,
            truncated: page.truncated,
            mode: mode.as_str(),
            returned: hits.len(),
            hits,
        })
    }

    /// Hit counts per child facet under `facet_prefix` (`/` = top-level categories,
    /// `/era`, `/author`, or any category path).
    pub fn facet_counts(&self, req: &SearchRequest, facet_prefix: &str) -> Result<Vec<FacetCount>> {
        let engine = self.lock();
        let prefix = facet_prefix.to_string();
        match req.effective_mode() {
            Mode::Exact => engine.get_facet_counts_exact(
                req.query.clone(),
                req.facets.clone(),
                prefix,
                req.match_nikud,
                req.match_taamim,
            ),
            Mode::Fuzzy => engine.get_facet_counts_fuzzy(
                req.query.clone(),
                req.facets.clone(),
                prefix,
                fuzzy_distance(req),
                req.match_nikud,
                req.match_taamim,
            ),
            Mode::Advanced => {
                let a = Advanced::from(req)?;
                engine.get_facet_counts_advanced(
                    req.query.clone(),
                    a.negative,
                    req.facets.clone(),
                    prefix,
                    a.distance,
                    a.distance,
                    HashMap::new(),
                    HashMap::new(),
                    HashMap::new(),
                    HashMap::new(),
                    a.options,
                    HashMap::new(),
                    req.match_nikud,
                    req.match_taamim,
                    a.scope,
                    SearchScope::WordDistance,
                    a.word_match.0,
                    a.word_match.1,
                )
            }
        }
        .map_err(|e| anyhow::anyhow!("tantivy facet count failed: {e:#}"))
    }

    /// Hit counts per indexed book (`file_path` → count).
    pub fn count_by_book(&self, req: &SearchRequest) -> Result<HashMap<String, u32>> {
        let engine = self.lock();
        match req.effective_mode() {
            Mode::Exact => engine.count_by_book_exact(
                req.query.clone(),
                req.facets.clone(),
                req.match_nikud,
                req.match_taamim,
            ),
            Mode::Fuzzy => engine.count_by_book_fuzzy(
                req.query.clone(),
                req.facets.clone(),
                fuzzy_distance(req),
                req.match_nikud,
                req.match_taamim,
            ),
            Mode::Advanced => {
                let a = Advanced::from(req)?;
                engine.count_by_book_advanced(
                    req.query.clone(),
                    a.negative,
                    req.facets.clone(),
                    a.distance,
                    a.distance,
                    HashMap::new(),
                    HashMap::new(),
                    HashMap::new(),
                    HashMap::new(),
                    a.options,
                    HashMap::new(),
                    req.match_nikud,
                    req.match_taamim,
                    a.scope,
                    SearchScope::WordDistance,
                    a.word_match.0,
                    a.word_match.1,
                )
            }
        }
        .map_err(|e| anyhow::anyhow!("tantivy count failed: {e:#}"))
    }
}

/// State of the engine's process-wide line source (for `library_info`).
pub fn line_source_state() -> serde_json::Value {
    let s = line_source_status();
    serde_json::json!({
        "configured": s.configured,
        "open": s.open,
        "suspended": s.suspend_depth > 0,
        "library_fallbacks": s.library_fallbacks,
    })
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    /// Total matching documents in the index (not just this page).
    pub total: u32,
    /// With `grouping`: the number of groups (paging counts groups).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_count: Option<u32>,
    /// A broad query overflowed its expansion budget: counts and hits are partial.
    pub truncated: bool,
    pub mode: &'static str,
    pub returned: usize,
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Serialize)]
pub struct SearchHit {
    /// Where this document lives. Only `official` hits can be followed with the
    /// SQLite-backed tools in this MCP.
    pub source: SearchHitSource,
    /// Canonical seforim.db book id for official text books only.
    pub book_id: Option<i64>,
    /// seforim.db line index for official text books only.
    pub line_index: Option<i64>,
    /// For PDF hits: 1-based page number (segment + 1).
    pub page: Option<i64>,
    pub is_pdf: bool,
    pub title: String,
    pub reference: String,
    /// Matched text with «…» around the matched term(s), HTML stripped.
    pub snippet: String,
    /// `ok`, `stale` (seforim.db changed since indexing; snippet = current line) or
    /// `unavailable` (the line could not be read back from seforim.db).
    pub text_status: &'static str,
    /// The phrase continues onto the next line (`line_index + 1`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub continues_to_next_line: bool,
    /// With `grouping`: raw hits merged into this one.
    #[serde(skip_serializing_if = "is_one")]
    pub merged_count: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub merged: Vec<MergedHit>,
    pub file_path: String,
}

fn is_one(n: &u32) -> bool {
    *n <= 1
}

#[derive(Debug, Serialize)]
pub struct MergedHit {
    pub book_id: Option<i64>,
    pub line_index: Option<i64>,
    pub title: String,
    pub reference: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchHitSource {
    Official,
    User,
    External,
    Pdf,
}

fn classify(
    file_path: &str,
    is_pdf: bool,
    segment: u64,
) -> (SearchHitSource, Option<i64>, Option<i64>, Option<i64>) {
    if is_pdf {
        (SearchHitSource::Pdf, None, None, Some(segment as i64 + 1))
    } else if let Some(book_id) = parse_official_book_id(file_path) {
        (
            SearchHitSource::Official,
            Some(book_id),
            Some(segment as i64),
            None,
        )
    } else if file_path.starts_with("uid:") {
        (SearchHitSource::User, None, None, None)
    } else {
        (SearchHitSource::External, None, None, None)
    }
}

impl SearchHit {
    fn from_result(r: SearchResult) -> SearchHit {
        let (source, book_id, line_index, page) = classify(&r.file_path, r.is_pdf, r.segment);
        SearchHit {
            source,
            book_id,
            line_index,
            page,
            is_pdf: r.is_pdf,
            title: r.title,
            reference: r.reference,
            snippet: clean_snippet(&r.text),
            text_status: match r.text_status {
                TextStatus::Ok => "ok",
                TextStatus::Stale => "stale",
                TextStatus::Unavailable => "unavailable",
            },
            continues_to_next_line: r.continues_to_next_line,
            merged_count: r.merged_count,
            merged: r
                .merged
                .into_iter()
                .map(|m| {
                    let (_, book_id, line_index, _) = classify(&m.file_path, m.is_pdf, m.segment);
                    MergedHit {
                        book_id,
                        line_index,
                        title: m.title,
                        reference: m.reference,
                    }
                })
                .collect(),
            file_path: r.file_path,
        }
    }
}

/// Parse an official seforim.db book id out of the index `file_path`.
pub fn parse_official_book_id(file_path: &str) -> Option<i64> {
    file_path.strip_prefix("id:")?.parse::<i64>().ok()
}

/// Convert the engine's `<font color=red>…</font>` highlight markup into `«…»`
/// markers, a cross-line `<br>` into ` / `, then strip any remaining HTML.
fn clean_snippet(text: &str) -> String {
    let marked = text
        .replace("<font color=red>", "«")
        .replace("</font>", "»")
        .replace("<br>", " / ");
    crate::text::strip_html_tags(&marked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use search_engine::api::search_engine::{SearchEngine, SearchResult, TextStatus};

    fn result(file_path: &str, is_pdf: bool, segment: u64) -> SearchResult {
        SearchResult {
            title: "ספר".to_string(),
            reference: String::new(),
            text: "<font color=red>א</font>ב".to_string(),
            id: 1,
            segment,
            is_pdf,
            file_path: file_path.to_string(),
            merged_count: 1,
            merged: Vec::new(),
            text_status: TextStatus::Ok,
            continues_to_next_line: false,
        }
    }

    #[test]
    fn parses_book_id_prefixes() {
        assert_eq!(parse_official_book_id("id:1"), Some(1));
        assert_eq!(parse_official_book_id("uid:57"), None);
        assert_eq!(parse_official_book_id("/abs/path/book.pdf"), None);
        assert_eq!(parse_official_book_id("ext:foo"), None);
    }

    #[test]
    fn mode_and_option_parsing() {
        assert_eq!(Mode::parse("exact").unwrap(), Mode::Exact);
        assert_eq!(Mode::parse("advanced").unwrap(), Mode::Advanced);
        assert_eq!(Mode::parse("fuzzy").unwrap(), Mode::Fuzzy);
        assert!(Mode::parse("nope").is_err());
        assert!(parse_order("generation").is_ok());
        assert!(parse_scope("same_section").is_ok());
        assert!(parse_grouping("identical_text").unwrap().is_some());
        assert!(matches!(
            parse_word_match("at_least:2").unwrap(),
            (Some(WordMatchMode::AtLeast), Some(2))
        ));
        assert!(parse_word_match("at_least:x").is_err());
    }

    #[test]
    fn exact_with_advanced_only_options_runs_advanced() {
        let mut req = SearchRequest::new("משה רבנו", Mode::Exact);
        assert_eq!(req.effective_mode(), Mode::Exact);
        req.options.prefixes = true;
        assert_eq!(req.effective_mode(), Mode::Advanced);
    }

    #[test]
    fn per_word_option_keys_follow_the_engine_format() {
        let opts = WordOptions {
            prefixes: true,
            ..WordOptions::default()
        };
        let map = per_word_options("משֶׁה רבנו", &opts);
        assert!(map.contains_key("משה_0"), "{map:?}");
        assert!(map.contains_key("רבנו_1"));
        assert_eq!(map["משה_0"].get("קידומות"), Some(&true));
    }

    #[test]
    fn results_are_classified_by_source() {
        let hit = SearchHit::from_result(result("uid:1", false, 7));
        assert_eq!(hit.source, SearchHitSource::User);
        assert_eq!(hit.book_id, None);
        let hit = SearchHit::from_result(result("id:5", false, 7));
        assert_eq!(
            (&hit.source, hit.book_id, hit.line_index),
            (&SearchHitSource::Official, Some(5), Some(7))
        );
        assert_eq!(hit.snippet, "«א»ב");
        let value = serde_json::to_value(&hit).unwrap();
        assert_eq!(value["is_pdf"], false);
        assert!(value.get("page").unwrap().is_null());
        let hit = SearchHit::from_result(result("/x.pdf", true, 0));
        assert!(hit.is_pdf);
        assert_eq!((hit.source, hit.page), (SearchHitSource::Pdf, Some(1)));
    }

    #[test]
    fn read_only_open_does_not_create_index_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        drop(SearchEngine::new(path));
        let metadata = dir.path().join("otzaria_index_meta.json");
        std::fs::remove_file(&metadata).unwrap();

        let db = dir.path().join("seforim.db");
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch("CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, content TEXT);")
            .unwrap();
        let _search = super::Search::open(dir.path(), None, &db, None).unwrap();
        assert!(!metadata.exists());
    }
}
