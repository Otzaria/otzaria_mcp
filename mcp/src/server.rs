//! MCP server surface (rmcp): read-only tools over the current library snapshot.
//! Every handler reads `self.lib.current()` (a cheap Arc clone) so a background
//! hot-reload swap never yields mixed DB/index state, and every response is stamped
//! with `schema_version` + `data_version` (the snapshot fingerprint).

use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use serde::Deserialize;
use serde_json::json;

use crate::db::{Db, Era};
use crate::reload::{Library, Snapshot};
use crate::search::{self, Mode, SearchRequest, WordOptions};

#[derive(Clone)]
pub struct OtzariaServer {
    lib: Arc<Library>,
}

// ── tool parameter schemas ───────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchBooksParams {
    /// Hebrew (or partial) book title, acronym/alias, or short-description text.
    pub query: String,
    /// Optional: only books in this category (and its sub-categories).
    #[serde(default)]
    pub category_id: Option<i64>,
    /// Max results to return (default 20, capped at 100).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LookupTitleParams {
    /// A book name, acronym or alias, e.g. "רמב\"ם", "שו\"ע או\"ח", "ספר בראשית", "מסכת ברכות".
    pub name: String,
    /// Max books (default 10, capped at 50).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetTextParams {
    /// Book id (from search_books / resolve_ref / full_text_search).
    pub book_id: i64,
    /// 0-based line index to start from (a line address, NOT a page offset). Default 0.
    #[serde(default)]
    pub line_index: Option<i64>,
    /// Number of lines to return (default 50, capped at 200).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Strip HTML tags from each line's content (default true).
    #[serde(default)]
    pub strip_html: Option<bool>,
    /// Optional text version/edition (a `version_title` or `he_version_title` from
    /// get_book_info.versions with has_text=true). Omit for the default text.
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BookIdParams {
    /// Book id.
    pub book_id: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WordOptionsParams {
    /// Allow Hebrew prefixes (ו/ה/ב/כ/ל/מ/ש…) before every word.
    #[serde(default)]
    pub prefixes: bool,
    /// Allow suffixes (possessive/plural endings) after every word.
    #[serde(default)]
    pub suffixes: bool,
    /// Richer grammatical prefixes (incl. Aramaic דא/כש…).
    #[serde(default)]
    pub grammatical_prefixes: bool,
    /// Grammatical suffixes.
    #[serde(default)]
    pub grammatical_suffixes: bool,
    /// Match full and deficient spelling (כתיב מלא/חסר).
    #[serde(default)]
    pub full_deficient_spelling: bool,
    /// Match the words as part of a longer word.
    #[serde(default)]
    pub partial_word: bool,
    /// Tolerate spelling mistakes (similar letters).
    #[serde(default)]
    pub typos: bool,
    /// Aramaic prefixes (ד/כד/אד/מד…).
    #[serde(default)]
    pub aramaic_prefixes: bool,
    /// Aramaic final-letter equivalences (ה↔א, ם↔ן).
    #[serde(default)]
    pub aramaic_suffixes: bool,
    /// Ignore geresh/gershayim typed in the words.
    #[serde(default)]
    pub ignore_quotes: bool,
    /// Expand acronyms both ways (needs Acronyms.json, see library_info).
    #[serde(default)]
    pub acronyms: bool,
    /// Expand words with their Aramaic↔Hebrew translations (needs dictionary.json).
    #[serde(default)]
    pub aramaic_translation: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct SearchFilterParams {
    /// Restrict to categories by title path, e.g. "תנ\"ך/תורה" or "/תלמוד בבלי".
    #[serde(default)]
    pub categories: Option<Vec<String>>,
    /// Restrict to categories by id (from browse_categories).
    #[serde(default)]
    pub category_ids: Option<Vec<i64>>,
    /// Restrict to specific books by id (search inside a book).
    #[serde(default)]
    pub book_ids: Option<Vec<i64>>,
    /// Restrict by era: "תורה שבכתב", "חז\"ל", "ראשונים", "אחרונים", "מחברי זמננו"
    /// (or torah/chazal/rishonim/acharonim/modern).
    #[serde(default)]
    pub eras: Option<Vec<String>>,
    /// Restrict by author name (as in get_book_info.authors).
    #[serde(default)]
    pub authors: Option<Vec<String>>,
    /// Only foundational (base) books.
    #[serde(default)]
    pub base_only: Option<bool>,
    /// Raw engine facet paths (advanced; e.g. "/תנ״ך/תורה", "/era/ראשונים", "/base").
    #[serde(default)]
    pub facets: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FullTextSearchParams {
    /// Hebrew search query. Nikud/te'amim are handled by the engine; quote a phrase to
    /// keep its words adjacent.
    pub query: String,
    /// Search mode: "advanced" (default; morphological options available), "exact",
    /// or "fuzzy" (edit distance per word, with lexical.db morphology when available).
    #[serde(default)]
    pub mode: Option<String>,
    /// Max results to return (default 20, capped at 100).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Pagination offset (default 0). With grouping, offset/limit count groups.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Result order: "catalogue" (default, library order), "relevance" or "generation"
    /// (chronological by era).
    #[serde(default)]
    pub order: Option<String>,
    /// advanced/exact: max words allowed between adjacent query words (default 0, as in
    /// the app). fuzzy: max edits per word, 0–2 (default 2).
    #[serde(default)]
    pub distance: Option<u32>,
    /// Advanced-mode word options, applied to every word of the query.
    #[serde(default)]
    pub options: Option<WordOptionsParams>,
    /// advanced: exclude lines matching this query (the app's "negative" search).
    #[serde(default)]
    pub exclude_query: Option<String>,
    /// advanced: "word_distance" (default, in order within `distance`), "same_paragraph"
    /// (all words in the same line, any order) or "same_section" (under the same heading).
    #[serde(default)]
    pub scope: Option<String>,
    /// advanced: how many query words must match — "all" (default), "any", "most",
    /// or "at_least:N".
    #[serde(default)]
    pub word_match: Option<String>,
    /// Require the nikud typed in the query.
    #[serde(default)]
    pub match_nikud: Option<bool>,
    /// Require the te'amim typed in the query.
    #[serde(default)]
    pub match_taamim: Option<bool>,
    /// "none" (default), "same_section" (one result per heading) or "identical_text"
    /// (merge identical lines across books).
    #[serde(default)]
    pub grouping: Option<String>,
    /// Legacy single facet path (same as one entry of filters.facets).
    #[serde(default)]
    pub facet: Option<String>,
    /// Scope filters (categories, books, eras, authors, base books).
    #[serde(default)]
    pub filters: Option<SearchFilterParams>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchStatsParams {
    /// Hebrew search query (same syntax as full_text_search).
    pub query: String,
    /// "category" (default: hit counts per sub-category of `category` / top level),
    /// "era", "author", or "book" (hit counts per book).
    #[serde(default)]
    pub by: Option<String>,
    /// For by=category: the category title path to break down (default: top level).
    #[serde(default)]
    pub category: Option<String>,
    /// Search mode: "advanced" (default), "exact" or "fuzzy".
    #[serde(default)]
    pub mode: Option<String>,
    /// Same as full_text_search.distance.
    #[serde(default)]
    pub distance: Option<u32>,
    /// Same as full_text_search.options.
    #[serde(default)]
    pub options: Option<WordOptionsParams>,
    /// Same as full_text_search.filters.
    #[serde(default)]
    pub filters: Option<SearchFilterParams>,
    /// Max rows (default 30, capped at 200).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetLinksParams {
    /// Book id to find cross-references for.
    pub book_id: i64,
    /// Optional 0-based line index (omit for the whole book).
    #[serde(default)]
    pub line_index: Option<i64>,
    /// Optional inclusive end line index for a range (default = line_index).
    #[serde(default)]
    pub end_line_index: Option<i64>,
    /// Optional connection-type filter, e.g. ["COMMENTARY","TARGUM"], ["LINKER"],
    /// ["QUOTATION","REFERENCE"], or ["SOURCE"] (incoming text-dependent links).
    #[serde(default)]
    pub connection_types: Option<Vec<String>>,
    /// Legacy single connection type (same as one entry of connection_types).
    #[serde(default)]
    pub connection_type: Option<String>,
    /// Optional: only links to these books.
    #[serde(default)]
    pub linked_book_ids: Option<Vec<i64>>,
    /// "outgoing" (default; the book is the stored source — e.g. its commentaries),
    /// "incoming" (the book is the stored target — e.g. what it comments on), or "both".
    #[serde(default)]
    pub direction: Option<String>,
    /// Max links to return (default 30, capped at 200).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Paging offset (default 0).
    #[serde(default)]
    pub offset: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ResolveRefParams {
    /// A human reference string, e.g. "בראשית א א", "בראשית פרק א פסוק א", "שו\"ע או\"ח א",
    /// "ברכות ב:", "ברכות ב ע\"א", "משנה ברכות א א", "בראשית נח" (parasha), or a book plus
    /// the opening words of a comment ("רש\"י על בראשית בראשית ברא").
    pub reference: String,
    /// Max candidate locations to return (default 10, capped at 50).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetCommentariesParams {
    /// Base-text book id.
    pub book_id: i64,
    /// 0-based start line index (default 0).
    #[serde(default)]
    pub line_index: Option<i64>,
    /// Optional inclusive end line index for a range (default = line_index).
    #[serde(default)]
    pub end_line_index: Option<i64>,
    /// Optional: restrict to specific commentator book ids.
    #[serde(default)]
    pub commentator_book_ids: Option<Vec<i64>>,
    /// Max commentary items to return (default 40, capped at 200).
    #[serde(default)]
    pub limit: Option<u32>,
    /// When a comment spans several lines, include up to this many (default 3, max 20).
    #[serde(default)]
    pub max_lines_per_item: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListCommentatorsParams {
    /// Base-text book id.
    pub book_id: i64,
    /// Optional 0-based line index (omit for the whole book).
    #[serde(default)]
    pub line_index: Option<i64>,
    /// Optional inclusive end line index (default = line_index).
    #[serde(default)]
    pub end_line_index: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LineParams {
    /// Book id.
    pub book_id: i64,
    /// 0-based line index.
    pub line_index: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetBookTocParams {
    /// Book id.
    pub book_id: i64,
    /// Optional alternate-structure key from `alt_structures`, e.g. `Parasha`.
    /// When set, `alt_entries` contains its navigable tree.
    #[serde(default)]
    pub alt_structure_key: Option<String>,
    /// Optional: only the children of this TOC entry id (lazy browsing of big TOCs).
    #[serde(default)]
    pub parent_id: Option<i64>,
    /// Max TOC entries (default 200, capped at 1000).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BrowseCategoriesParams {
    /// Category id to open; omit (and omit path) for the top-level categories.
    #[serde(default)]
    pub category_id: Option<i64>,
    /// Alternatively, a category title path, e.g. "תנ\"ך/תורה".
    #[serde(default)]
    pub path: Option<String>,
    /// Max books to list directly under the category (default 50, capped at 200).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NoParams {}

// ── helpers for search params ────────────────────────────────────────────────

fn word_options(p: Option<WordOptionsParams>) -> WordOptions {
    let Some(p) = p else {
        return WordOptions::default();
    };
    WordOptions {
        prefixes: p.prefixes,
        suffixes: p.suffixes,
        grammatical_prefixes: p.grammatical_prefixes,
        grammatical_suffixes: p.grammatical_suffixes,
        full_deficient_spelling: p.full_deficient_spelling,
        partial_word: p.partial_word,
        typos: p.typos,
        aramaic_prefixes: p.aramaic_prefixes,
        aramaic_suffixes: p.aramaic_suffixes,
        ignore_quotes: p.ignore_quotes,
        acronyms: p.acronyms,
        aramaic_translation: p.aramaic_translation,
    }
}

/// A dimension facet value: '/' would read as hierarchy (the app's sanitisation).
fn facet_segment(s: &str) -> String {
    s.replace('/', "\u{05BE}").trim().to_string()
}

/// Turn the user-facing filters into engine facet paths.
fn build_facets(db: &Db, f: Option<SearchFilterParams>, legacy: Option<String>) -> anyhow::Result<Vec<String>> {
    let f = f.unwrap_or_default();
    let mut facets: Vec<String> = f.facets.unwrap_or_default();
    facets.extend(legacy);
    for c in f.categories.unwrap_or_default() {
        match db.category_by_path(&c)? {
            Some(id) => facets.extend(db.category_facet(id)?),
            None => anyhow::bail!("no category at path {c:?} (see browse_categories)"),
        }
    }
    for id in f.category_ids.unwrap_or_default() {
        match db.category_facet(id)? {
            Some(p) => facets.push(p),
            None => anyhow::bail!("no category with category_id={id}"),
        }
    }
    for id in f.book_ids.unwrap_or_default() {
        let info = db
            .get_book_info(id)?
            .ok_or_else(|| anyhow::anyhow!("no book found with book_id={id}"))?;
        let cat = db.category_facet(info.category_id)?.unwrap_or_default();
        facets.push(format!("{cat}/id:{id}"));
    }
    for e in f.eras.unwrap_or_default() {
        let era = Era::parse(&e).ok_or_else(|| anyhow::anyhow!("unknown era {e:?}"))?;
        facets.push(format!("/era/{}", facet_segment(era.hebrew_name())));
    }
    for a in f.authors.unwrap_or_default() {
        facets.push(format!("/author/{}", facet_segment(&a)));
    }
    if f.base_only.unwrap_or(false) {
        facets.push("/base".to_string());
    }
    Ok(facets
        .into_iter()
        .map(|p| if p.starts_with('/') { p } else { format!("/{p}") })
        .collect())
}

fn invalid(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::invalid_params(e.to_string(), None)
}

// ── tools ─────────────────────────────────────────────────────────────────────

#[tool_router]
impl OtzariaServer {
    pub fn new(lib: Arc<Library>) -> Self {
        Self { lib }
    }

    #[tool(
        name = "library_info",
        description = "Status of the Otzaria library this server reads: seforim.db content version \
                       (db_version) and table-layout version, book/line counts, connection types, which \
                       optional tables exist, and the full-text index (schema, engine version, documents, \
                       dictionaries) — or why search is unavailable.",
        annotations(read_only_hint = true)
    )]
    pub async fn library_info(
        &self,
        Parameters(_p): Parameters<NoParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let snap = self.lib.current();
        let db = snap.db.clone();
        let stats = run_blocking(move || db.library_stats()).await?;
        let index = match &snap.search {
            Some(s) => json!({ "available": true, "info": s.info, "line_source": search::line_source_state() }),
            None => json!({ "available": false, "reason": snap.search_error }),
        };
        self.respond(
            &snap,
            json!({
                "db_version": snap.db.schema_version(),
                "db_schema_version": snap.db.db_schema_version(),
                "stats": stats,
                "capabilities": snap.db.caps,
                "index": index,
            }),
        )
    }

    #[tool(
        name = "search_books",
        description = "Search the Otzaria library for books by Hebrew title, acronym/alias or short \
                       description (best matches first: exact title, exact alias, title prefix, …). Returns \
                       book id, title, heRef, category, author(s), era, line count and a short description. \
                       Optionally restrict to a category. Use book_id with get_text / get_book_info.",
        annotations(read_only_hint = true)
    )]
    pub async fn search_books(
        &self,
        Parameters(p): Parameters<SearchBooksParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let query = required_text(p.query, "query")?;
        let snap = self.lib.current();
        let limit = p.limit.unwrap_or(20).clamp(1, 100) as i64;
        let db = snap.db.clone();
        let cat = p.category_id;
        let books = run_blocking(move || db.search_books(&query, cat, limit)).await?;
        self.respond(&snap, json!({ "count": books.len(), "books": books }))
    }

    #[tool(
        name = "lookup_title",
        description = "Resolve a book name, acronym or alias to books — e.g. \"רמב\\\"ם\", \"שו\\\"ע או\\\"ח\", \
                       \"ספר בראשית\", \"מסכת ברכות\" — using the same normalized name index as resolve_ref \
                       (titles, heRefs and all book_acronym terms; nikud/gershayim ignored). Exact matches \
                       first, then names starting with it; base texts before commentaries.",
        annotations(read_only_hint = true)
    )]
    pub async fn lookup_title(
        &self,
        Parameters(p): Parameters<LookupTitleParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let name = required_text(p.name, "name")?;
        let snap = self.lib.current();
        let limit = p.limit.unwrap_or(10).clamp(1, 50) as i64;
        let db = snap.db.clone();
        let books = run_blocking(move || db.lookup_title(&name, limit)).await?;
        self.respond(&snap, json!({ "count": books.len(), "books": books }))
    }

    #[tool(
        name = "get_text",
        description = "Retrieve lines of a book by book_id starting at a 0-based line_index (decodes the \
                       library's compressed text). Returns each line's heRef and content, total_lines, and \
                       next_line_index for paging. line_index is a line address (the value full_text_search / \
                       resolve_ref / get_book_toc return), not a page number. Pass `version` to read another \
                       edition listed in get_book_info.versions.",
        annotations(read_only_hint = true)
    )]
    pub async fn get_text(
        &self,
        Parameters(p): Parameters<GetTextParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let snap = self.lib.current();
        let line_index = p.line_index.unwrap_or(0).max(0);
        let limit = p.limit.unwrap_or(50).clamp(1, 200) as i64;
        let strip_html = p.strip_html.unwrap_or(true);
        let book_id = p.book_id;
        let version = p.version.filter(|v| !v.trim().is_empty());
        let db = snap.db.clone();
        let result = run_blocking(move || {
            db.get_text(book_id, line_index, limit, strip_html, version.as_deref())
        })
        .await?;
        match result {
            Some(r) => self.respond(&snap, to_json(r)?),
            None => Err(invalid(format!("no book found with book_id={book_id}"))),
        }
    }

    #[tool(
        name = "get_book_info",
        description = "Book details for a book_id: title, heRef, category path, short and long \
                       description, source, author(s), generation and era, publication dates and places, \
                       topics, dependence type and collective title, base texts it depends on, default \
                       commentators/targumim, connection flags, nikud/te'amim flags, acronyms/aliases, \
                       alternate structures, and the available text versions/editions.",
        annotations(read_only_hint = true)
    )]
    pub async fn get_book_info(
        &self,
        Parameters(p): Parameters<BookIdParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let snap = self.lib.current();
        let book_id = p.book_id;
        let db = snap.db.clone();
        let info = run_blocking(move || db.get_book_info(book_id)).await?;
        match info {
            Some(i) => self.respond(&snap, to_json(i)?),
            None => Err(invalid(format!("no book found with book_id={book_id}"))),
        }
    }

    #[tool(
        name = "full_text_search",
        description = "Full-text search across the Otzaria library with its real Tantivy engine (the \
                       app's engine and options: Hebrew morphology, nikud/te'amim, phrases across line \
                       breaks). Modes: 'advanced' (default; word options: prefixes, suffixes, spelling, \
                       typos, partial word, Aramaic, acronyms, translation; scope; word_match; exclude_query), \
                       'exact', 'fuzzy'. Filters by category, book, era, author, base books. Paging with \
                       offset/limit; optional grouping. Official hits return canonical book_id + line_index \
                       (feed into get_text / get_links / get_commentaries); user, external and PDF hits are \
                       marked by source.",
        annotations(read_only_hint = true)
    )]
    pub async fn full_text_search(
        &self,
        Parameters(p): Parameters<FullTextSearchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let query = required_text(p.query, "query")?;
        let snap = self.lib.current();
        let search = self.search_of(&snap)?;
        let mode = Mode::parse(p.mode.as_deref().unwrap_or("advanced")).map_err(invalid)?;
        let mut req = SearchRequest::new(query, mode);
        req.limit = p.limit.unwrap_or(20).clamp(1, 100);
        req.offset = p.offset.unwrap_or(0);
        req.order = p.order.unwrap_or_else(|| "catalogue".to_string());
        req.distance = p.distance;
        req.options = word_options(p.options);
        req.negative_query = p.exclude_query;
        req.scope = p.scope.unwrap_or_else(|| "word_distance".to_string());
        req.word_match = p.word_match.unwrap_or_else(|| "all".to_string());
        req.match_nikud = p.match_nikud.unwrap_or(false);
        req.match_taamim = p.match_taamim.unwrap_or(false);
        req.grouping = p.grouping.unwrap_or_else(|| "none".to_string());
        let db = snap.db.clone();
        let filters = p.filters;
        let legacy = p.facet;
        let resp = run_blocking(move || {
            req.facets = build_facets(&db, filters, legacy)?;
            let facets = req.facets.clone();
            Ok((search.search(&req)?, facets))
        })
        .await?;
        let (resp, facets) = resp;
        let mut v = to_json(resp)?;
        if !facets.is_empty() {
            v["facets"] = json!(facets);
        }
        self.respond(&snap, v)
    }

    #[tool(
        name = "search_facet_counts",
        description = "Where does a query occur? Hit counts of a full-text query broken down by category \
                       (sub-categories of `category`, or top level), by era, by author, or by book — like \
                       the app's search-scope counts. Accepts the same mode/options/filters as \
                       full_text_search. Use it to choose filters before paging through hits.",
        annotations(read_only_hint = true)
    )]
    pub async fn search_facet_counts(
        &self,
        Parameters(p): Parameters<SearchStatsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let query = required_text(p.query, "query")?;
        let snap = self.lib.current();
        let search = self.search_of(&snap)?;
        let mode = Mode::parse(p.mode.as_deref().unwrap_or("advanced")).map_err(invalid)?;
        let by = p.by.unwrap_or_else(|| "category".to_string());
        let limit = p.limit.unwrap_or(30).clamp(1, 200) as usize;
        let mut req = SearchRequest::new(query, mode);
        req.distance = p.distance;
        req.options = word_options(p.options);
        let db = snap.db.clone();
        let filters = p.filters;
        let category = p.category;
        let value = run_blocking(move || {
            req.facets = build_facets(&db, filters, None)?;
            match by.as_str() {
                "book" => {
                    let counts = search.count_by_book(&req)?;
                    let mut rows: Vec<(String, u32)> = counts.into_iter().collect();
                    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                    let total_books = rows.len();
                    rows.truncate(limit);
                    let ids: Vec<i64> = rows
                        .iter()
                        .filter_map(|(fp, _)| search::parse_official_book_id(fp))
                        .collect();
                    let titles = db.book_titles(&ids)?;
                    let books: Vec<serde_json::Value> = rows
                        .into_iter()
                        .map(|(fp, n)| {
                            let id = search::parse_official_book_id(&fp);
                            json!({
                                "book_id": id,
                                "title": id.and_then(|i| titles.get(&i).cloned()),
                                "file_path": fp,
                                "hits": n,
                            })
                        })
                        .collect();
                    Ok(json!({ "by": "book", "books_with_hits": total_books, "rows": books }))
                }
                "category" | "era" | "author" => {
                    let prefix = match by.as_str() {
                        "era" => "/era".to_string(),
                        "author" => "/author".to_string(),
                        _ => match category.as_deref().map(str::trim).filter(|c| !c.trim_matches('/').is_empty()) {
                            None => "/".to_string(),
                            Some(c) => match db.category_by_path(c)? {
                                Some(id) => db.category_facet(id)?.unwrap_or_else(|| "/".into()),
                                None => anyhow::bail!("no category at path {c:?}"),
                            },
                        },
                    };
                    let mut counts = search.facet_counts(&req, &prefix)?;
                    counts.sort_by(|a, b| b.count.cmp(&a.count).then(a.path.cmp(&b.path)));
                    let total = counts.len();
                    let rows: Vec<serde_json::Value> = counts
                        .into_iter()
                        .take(limit)
                        .map(|c| {
                            let name = c.path.rsplit('/').next().unwrap_or_default().to_string();
                            json!({ "facet": c.path, "name": name, "hits": c.count })
                        })
                        .collect();
                    Ok(json!({ "by": by, "prefix": prefix, "facets_with_hits": total, "rows": rows }))
                }
                other => anyhow::bail!("unknown by '{other}' (expected: category | era | author | book)"),
            }
        })
        .await?;
        self.respond(&snap, value)
    }

    #[tool(
        name = "get_links",
        description = "Cross-references (links) for a book, a line or a line range, following the app's \
                       link model: outgoing = the book is the stored source (e.g. its commentaries), incoming \
                       = the book is the target (a commentary's base text appears as display_type SOURCE). \
                       Covers all connection types in the library (COMMENTARY, SUPER_COMMENTARY, TARGUM, \
                       MIDRASH, PARSHANUT, DIBUR_HAMATCHIL, ELUCIDATION, FOOTNOTES, REFERENCE, QUOTATION, \
                       MESORAT_HASHAS, EIN_MISHPAT, MISHNAH_IN_TALMUD, RELATED, LINKER, LAW, …), range links \
                       (link_range / link_coverage), suppressed sides, and word-level anchors. Each link gives \
                       the linked book_id + line_index (+ range end) + heRef + snippet.",
        annotations(read_only_hint = true)
    )]
    pub async fn get_links(
        &self,
        Parameters(p): Parameters<GetLinksParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let snap = self.lib.current();
        let book_id = p.book_id;
        let window = p.line_index.map(|s| {
            let s = s.max(0);
            (s, p.end_line_index.unwrap_or(s).max(s))
        });
        let mut types: Vec<String> = p.connection_types.unwrap_or_default();
        types.extend(p.connection_type);
        let types: Vec<String> = types.into_iter().map(|t| t.trim().to_uppercase()).collect();
        let linked = p.linked_book_ids;
        let direction = p.direction.unwrap_or_else(|| "outgoing".to_string());
        let limit = p.limit.unwrap_or(30).clamp(1, 200) as i64;
        let offset = p.offset.unwrap_or(0) as i64;
        let db = snap.db.clone();
        let links = run_blocking(move || {
            let types = (!types.is_empty()).then_some(types);
            db.get_links(
                book_id,
                window,
                types.as_deref(),
                linked.as_deref(),
                &direction,
                limit,
                offset,
            )
        })
        .await?;
        self.respond(&snap, json!({ "count": links.len(), "links": links }))
    }

    #[tool(
        name = "resolve_ref",
        description = "Find-ref: resolve a Hebrew reference to canonical (book_id, line_index), with the \
                       app's logic — book names, acronyms and aliases; the canonical reference key looked up \
                       in the library's line_ref index (\"בראשית א, א\", \"פרק א פסוק א\", \"ברכות ב.\", \"ב ע\\\"א\", \
                       \"טור שט ג\" with named parts omitted); chapter/daf-level prefixes; TOC and parasha \
                       headings; and a commentary's dibbur hamatchil. Feed a candidate into get_text / get_links.",
        annotations(read_only_hint = true)
    )]
    pub async fn resolve_ref(
        &self,
        Parameters(p): Parameters<ResolveRefParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let reference = required_text(p.reference, "reference")?;
        let snap = self.lib.current();
        let limit = p.limit.unwrap_or(10).clamp(1, 50) as i64;
        let db = snap.db.clone();
        let candidates = run_blocking(move || db.resolve_ref(&reference, limit)).await?;
        self.respond(
            &snap,
            json!({ "count": candidates.len(), "candidates": candidates }),
        )
    }

    #[tool(
        name = "get_commentaries",
        description = "Commentaries, targumim, midrash and other text-dependent works on a book's line \
                       (or line range): the commentary text itself (multi-line comments joined), with its \
                       commentator book_id + line_index + heRef + era and the word anchor in the base line, \
                       plus the book's ordered default commentators and targumim. Optionally restrict to \
                       specific commentator book ids (see list_commentators).",
        annotations(read_only_hint = true)
    )]
    pub async fn get_commentaries(
        &self,
        Parameters(p): Parameters<GetCommentariesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let snap = self.lib.current();
        let start = p.line_index.unwrap_or(0).max(0);
        let end = p.end_line_index.unwrap_or(start).max(start);
        let limit = p.limit.unwrap_or(40).clamp(1, 200) as i64;
        let max_lines = p.max_lines_per_item.unwrap_or(3).clamp(1, 20) as i64;
        let ids = p.commentator_book_ids;
        let book_id = p.book_id;
        let db = snap.db.clone();
        let res = run_blocking(move || {
            db.get_commentaries(book_id, start, end, ids.as_deref(), limit, max_lines)
        })
        .await?;
        self.respond(&snap, to_json(res)?)
    }

    #[tool(
        name = "list_commentators",
        description = "Which commentators (text-dependent works) are linked to a book — or to a line / \
                       line range — with link counts per connection type, grouped and ordered by era \
                       (תורה שבכתב, חז\"ל, ראשונים, אחרונים, מחברי זמננו, others) like the app's commentators \
                       panel; default commentators/targumim first within each era.",
        annotations(read_only_hint = true)
    )]
    pub async fn list_commentators(
        &self,
        Parameters(p): Parameters<ListCommentatorsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let snap = self.lib.current();
        let window = p.line_index.map(|s| {
            let s = s.max(0);
            (s, p.end_line_index.unwrap_or(s).max(s))
        });
        let book_id = p.book_id;
        let db = snap.db.clone();
        let res = run_blocking(move || db.list_commentators(book_id, window)).await?;
        self.respond(&snap, to_json(res)?)
    }

    #[tool(
        name = "get_line_anchors",
        description = "Word-level link anchors inside one line (link_anchor): for each anchor, its \
                       visible-character range, the anchored words (or the text around a point marker such \
                       as a commentary letter), its label, and the linked book/line/heRef. Returns the line's \
                       visible text too, so offsets can be checked.",
        annotations(read_only_hint = true)
    )]
    pub async fn get_line_anchors(
        &self,
        Parameters(p): Parameters<LineParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let snap = self.lib.current();
        let (book_id, line_index) = (p.book_id, p.line_index);
        let db = snap.db.clone();
        let res = run_blocking(move || db.get_line_anchors(book_id, line_index)).await?;
        match res {
            Some(r) => self.respond(&snap, to_json(r)?),
            None => Err(invalid(format!(
                "no line {line_index} in book_id={book_id}"
            ))),
        }
    }

    #[tool(
        name = "get_book_toc",
        description = "Table of contents for a book: a flattened, pre-ordered list of entries with id, \
                       parent_id, level, text, has_children, and a line_index pointer for get_text. Pass \
                       parent_id to list one entry's children. Also lists the book's alternate structures \
                       (parasha, daf, …); pass alt_structure_key to return that structure's entries.",
        annotations(read_only_hint = true)
    )]
    pub async fn get_book_toc(
        &self,
        Parameters(p): Parameters<GetBookTocParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let snap = self.lib.current();
        let limit = p.limit.unwrap_or(200).clamp(1, 1000) as i64;
        let book_id = p.book_id;
        let alt_structure_key = p.alt_structure_key;
        let parent_id = p.parent_id;
        let db = snap.db.clone();
        let toc = run_blocking(move || {
            db.get_book_toc(book_id, limit, alt_structure_key.as_deref(), parent_id)
        })
        .await?;
        self.respond(&snap, to_json(toc)?)
    }

    #[tool(
        name = "browse_categories",
        description = "Navigate the Otzaria category tree. With no category, returns the top-level \
                       categories. With a category_id or a title path (\"תנ\\\"ך/תורה\"), returns its description \
                       and path, its child categories (with recursive book counts) and the books directly \
                       under it. Use has_children to drill down and book_id with get_text / get_book_info.",
        annotations(read_only_hint = true)
    )]
    pub async fn browse_categories(
        &self,
        Parameters(p): Parameters<BrowseCategoriesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let snap = self.lib.current();
        let limit = p.limit.unwrap_or(50).clamp(1, 200) as i64;
        let category_id = p.category_id;
        let path = p.path;
        let db = snap.db.clone();
        let view =
            run_blocking(move || db.browse_categories(category_id, path.as_deref(), limit)).await?;
        self.respond(&snap, to_json(view)?)
    }

    fn search_of(&self, snap: &Snapshot) -> Result<Arc<search::Search>, ErrorData> {
        snap.search.clone().ok_or_else(|| {
            ErrorData::internal_error(
                format!(
                    "full-text search is unavailable: {}",
                    snap.search_error.as_deref().unwrap_or("no Tantivy index")
                ),
                None,
            )
        })
    }

    /// Stamp a tool payload with snapshot identity (schema_version, data_version)
    /// and return it as compact JSON text content.
    fn respond(
        &self,
        snap: &Snapshot,
        mut payload: serde_json::Value,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("schema_version".into(), json!(snap.db.schema_version()));
            obj.insert("data_version".into(), json!(snap.fingerprint));
        }
        let text = serde_json::to_string(&payload)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }
}

// ── server handler / capabilities ─────────────────────────────────────────────

#[tool_handler]
impl ServerHandler for OtzariaServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("otzaria-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Offline, read-only access to the Otzaria Jewish-texts library (SQLite seforim.db + the \
                 app's Tantivy index). Find a book with lookup_title / search_books / browse_categories or a \
                 location with resolve_ref (find-ref); read with get_text (line_index is 0-based) and \
                 navigate with get_book_toc; explore connections with list_commentators, get_commentaries, \
                 get_links and get_line_anchors; search the whole library with full_text_search (and \
                 search_facet_counts to see where a phrase occurs); get_book_info has authors, era, \
                 publication data and descriptions; library_info reports versions and status. Every \
                 response includes schema_version (library db_version) and data_version (snapshot \
                 fingerprint).",
            )
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn to_json<T: serde::Serialize>(v: T) -> Result<serde_json::Value, ErrorData> {
    serde_json::to_value(v).map_err(|e| ErrorData::internal_error(e.to_string(), None))
}

/// Run a blocking DB/index call off the async runtime, mapping errors to MCP errors.
async fn run_blocking<T, F>(f: F) -> Result<T, ErrorData>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ErrorData::internal_error(format!("task join error: {e}"), None))?
        .map_err(|e| ErrorData::internal_error(format!("{e:#}"), None))
}

fn required_text(value: String, field: &str) -> Result<String, ErrorData> {
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(ErrorData::invalid_params(
            format!("{field} must not be empty"),
            None,
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::{facet_segment, required_text};

    #[test]
    fn rejects_blank_required_text() {
        assert!(required_text("  \n".to_string(), "query").is_err());
        assert_eq!(
            required_text(" בראשית ".to_string(), "query").unwrap(),
            "בראשית"
        );
    }

    #[test]
    fn dimension_facet_segments_are_sanitised() {
        assert_eq!(facet_segment("a/b"), "a\u{05BE}b");
    }
}
