//! MCP server surface (rmcp): 9 read-only tools over the current library snapshot.
//! Every handler reads `self.lib.current()` (a cheap Arc clone) so a background
//! hot-reload swap never yields mixed DB/index state, and every response is stamped
//! with `schema_version` + `data_version` (the snapshot fingerprint).

use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use serde::Deserialize;
use serde_json::json;

use crate::reload::{Library, Snapshot};
use crate::search::Mode;

#[derive(Clone)]
pub struct OtzariaServer {
    lib: Arc<Library>,
}

// ── tool parameter schemas ───────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchBooksParams {
    /// Hebrew (or partial) book title, or short-description text, to search for.
    pub query: String,
    /// Max results to return (default 20, capped at 100).
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
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetBookInfoParams {
    /// Book id to describe.
    pub book_id: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FullTextSearchParams {
    /// Hebrew search query. Nikud/te'amim are handled by the engine.
    pub query: String,
    /// Search mode: "advanced" (default, morphological), "exact", or "fuzzy".
    #[serde(default)]
    pub mode: Option<String>,
    /// Max results to return (default 20, capped at 100).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Pagination offset (default 0).
    #[serde(default)]
    pub offset: Option<u32>,
    /// Optional topics-facet path to restrict scope, e.g. "/תנ\"ך/תורה".
    #[serde(default)]
    pub facet: Option<String>,
    /// Result order: "catalogue" (default, library order) or "relevance".
    #[serde(default)]
    pub order: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetLinksParams {
    /// Book id to find cross-references for.
    pub book_id: i64,
    /// Optional: restrict to a single 0-based line index within the book.
    #[serde(default)]
    pub line_index: Option<i64>,
    /// Optional connection-type filter, e.g. COMMENTARY, SUPER_COMMENTARY, TARGUM,
    /// SOURCE, REFERENCE, MIDRASH, QUOTATION, PARSHANUT, DIBUR_HAMATCHIL.
    #[serde(default)]
    pub connection_type: Option<String>,
    /// "outgoing" (default; the book is the link source), "incoming" (the book is
    /// the target — e.g. commentaries pointing at it), or "both".
    #[serde(default)]
    pub direction: Option<String>,
    /// Max links to return (default 30, capped at 200).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ResolveRefParams {
    /// A human reference string, e.g. "בראשית א א", "שו\"ע או\"ח א", "ברכות ב:",
    /// "משנה ברכות א א". Nikud, gershayim, and Talmud amud markers (. / :) are handled.
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
    /// Max commentary lines to return (default 40, capped at 200).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetBookTocParams {
    /// Book id.
    pub book_id: i64,
    /// Max TOC entries (default 200, capped at 1000).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BrowseCategoriesParams {
    /// Category id to open; omit for the top-level (root) categories.
    #[serde(default)]
    pub category_id: Option<i64>,
    /// Max books to list directly under the category (default 50, capped at 200).
    #[serde(default)]
    pub limit: Option<u32>,
}

// ── tools ─────────────────────────────────────────────────────────────────────

#[tool_router]
impl OtzariaServer {
    pub fn new(lib: Arc<Library>) -> Self {
        Self { lib }
    }

    #[tool(
        name = "search_books",
        description = "Search the Otzaria library for books by Hebrew title or short description. \
                       Returns book id, title, canonical heRef, category, author(s), and line count. \
                       Use the returned book_id with get_text or get_book_info.",
        annotations(read_only_hint = true)
    )]
    pub async fn search_books(
        &self,
        Parameters(p): Parameters<SearchBooksParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let snap = self.lib.current();
        let limit = p.limit.unwrap_or(20).min(100) as i64;
        let db = snap.db.clone();
        let query = p.query;
        let books = run_blocking(move || db.search_books(&query, limit)).await?;
        self.respond(&snap, json!({ "count": books.len(), "books": books }))
    }

    #[tool(
        name = "get_text",
        description = "Retrieve lines of a book by book_id starting at a 0-based line_index. \
                       Returns each line's heRef and content, plus next_line_index for paging. \
                       line_index is a line address (the same value full_text_search/resolve_ref return), not a page number.",
        annotations(read_only_hint = true)
    )]
    pub async fn get_text(
        &self,
        Parameters(p): Parameters<GetTextParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let snap = self.lib.current();
        let line_index = p.line_index.unwrap_or(0).max(0);
        let limit = p.limit.unwrap_or(50).min(200) as i64;
        let strip_html = p.strip_html.unwrap_or(true);
        let book_id = p.book_id;
        let db = snap.db.clone();
        let result =
            run_blocking(move || db.get_text(book_id, line_index, limit, strip_html)).await?;
        match result {
            Some(r) => self.respond(&snap, serde_json::to_value(r).unwrap()),
            None => Err(ErrorData::invalid_params(
                format!("no book found with book_id={book_id}"),
                None,
            )),
        }
    }

    #[tool(
        name = "get_book_info",
        description = "Get full metadata for a book_id: title, heRef, category, descriptions, source, \
                       author(s), historical generation/era, topics, connection flags, alternate-structure \
                       and nikud/te'amim flags, sample acronyms/aliases, and available text versions.",
        annotations(read_only_hint = true)
    )]
    pub async fn get_book_info(
        &self,
        Parameters(p): Parameters<GetBookInfoParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let snap = self.lib.current();
        let book_id = p.book_id;
        let db = snap.db.clone();
        let info = run_blocking(move || db.get_book_info(book_id)).await?;
        match info {
            Some(i) => self.respond(&snap, serde_json::to_value(i).unwrap()),
            None => Err(ErrorData::invalid_params(
                format!("no book found with book_id={book_id}"),
                None,
            )),
        }
    }

    #[tool(
        name = "full_text_search",
        description = "Full-text search across the entire Otzaria library using its real Tantivy engine \
                       (Hebrew morphology; nikud/te'amim handled automatically). Modes: 'advanced' (default, \
                       morphological), 'exact', 'fuzzy'. Each hit returns canonical book_id + line_index \
                       (feed straight into get_text / get_links / get_book_info), the book title, a reference \
                       breadcrumb, and a «…»-highlighted snippet. PDF hits return a page number instead.",
        annotations(read_only_hint = true)
    )]
    pub async fn full_text_search(
        &self,
        Parameters(p): Parameters<FullTextSearchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let snap = self.lib.current();
        let Some(search) = snap.search.clone() else {
            return Err(ErrorData::internal_error(
                "full-text search is unavailable: no Tantivy index was found (pass --index or --data-dir)",
                None,
            ));
        };
        let mode = Mode::parse(p.mode.as_deref().unwrap_or("advanced"))
            .map_err(|e| ErrorData::invalid_params(e.to_string(), None))?;
        let limit = p.limit.unwrap_or(20).min(100);
        let offset = p.offset.unwrap_or(0);
        let order = p.order.unwrap_or_else(|| "catalogue".to_string());
        let query = p.query;
        let facet = p.facet;
        let resp =
            run_blocking(move || search.search(query, mode, limit, offset, facet, &order)).await?;
        self.respond(&snap, serde_json::to_value(resp).unwrap())
    }

    #[tool(
        name = "get_links",
        description = "Cross-references (links) for a book, optionally a single line. Otzaria has 15 connection \
                       types (COMMENTARY, SUPER_COMMENTARY, TARGUM, SOURCE, REFERENCE, MIDRASH, QUOTATION, \
                       PARSHANUT, DIBUR_HAMATCHIL, MESORAT_HASHAS, EIN_MISHPAT, MISHNAH_IN_TALMUD, RELATED, \
                       OTHER, LINKER). direction: 'outgoing' (default), 'incoming' (e.g. commentaries on this \
                       book), or 'both'. Each link gives source and target book_id + line_index + heRef + snippet.",
        annotations(read_only_hint = true)
    )]
    pub async fn get_links(
        &self,
        Parameters(p): Parameters<GetLinksParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let snap = self.lib.current();
        let book_id = p.book_id;
        let line_index = p.line_index;
        let connection_type = p.connection_type;
        let direction = p.direction.unwrap_or_else(|| "outgoing".to_string());
        let limit = p.limit.unwrap_or(30).min(200) as i64;
        let db = snap.db.clone();
        let links = run_blocking(move || {
            db.get_links(
                book_id,
                line_index,
                connection_type.as_deref(),
                &direction,
                limit,
            )
        })
        .await?;
        self.respond(&snap, json!({ "count": links.len(), "links": links }))
    }

    #[tool(
        name = "resolve_ref",
        description = "Resolve a human Hebrew reference to canonical (book_id, line_index) locations. \
                       Handles book titles, abbreviations/nicknames (book_acronym), Tanakh (\"בראשית א א\"), \
                       Mishnah/Talmud incl. daf notation (\"ברכות ב:\"), and works across books sharing a name \
                       (returns the one whose reference actually matches). Feed a candidate's book_id + \
                       line_index into get_text / get_links.",
        annotations(read_only_hint = true)
    )]
    pub async fn resolve_ref(
        &self,
        Parameters(p): Parameters<ResolveRefParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let snap = self.lib.current();
        let reference = p.reference;
        let limit = p.limit.unwrap_or(10).min(50) as i64;
        let db = snap.db.clone();
        let candidates = run_blocking(move || db.resolve_ref(&reference, limit)).await?;
        self.respond(
            &snap,
            json!({ "count": candidates.len(), "candidates": candidates }),
        )
    }

    #[tool(
        name = "get_commentaries",
        description = "Commentaries and targumim on a book's line (or a line range): returns the actual \
                       commentary text with its commentator book_id + line_index + heRef, plus the book's \
                       ordered default-commentator list. Optionally restrict to specific commentator book ids.",
        annotations(read_only_hint = true)
    )]
    pub async fn get_commentaries(
        &self,
        Parameters(p): Parameters<GetCommentariesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let snap = self.lib.current();
        let start = p.line_index.unwrap_or(0).max(0);
        let end = p.end_line_index.unwrap_or(start).max(start);
        let limit = p.limit.unwrap_or(40).min(200) as i64;
        let ids = p.commentator_book_ids;
        let book_id = p.book_id;
        let db = snap.db.clone();
        let res =
            run_blocking(move || db.get_commentaries(book_id, start, end, ids.as_deref(), limit))
                .await?;
        self.respond(&snap, serde_json::to_value(res).unwrap())
    }

    #[tool(
        name = "get_book_toc",
        description = "Hierarchical table of contents for a book: a flattened, pre-ordered list of entries with \
                       level, text, has_children, and a line_index pointer to pass to get_text. Also lists the \
                       book's alternate structures (e.g. Parasha, Daf, Siman) when present.",
        annotations(read_only_hint = true)
    )]
    pub async fn get_book_toc(
        &self,
        Parameters(p): Parameters<GetBookTocParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let snap = self.lib.current();
        let limit = p.limit.unwrap_or(200).min(1000) as i64;
        let book_id = p.book_id;
        let db = snap.db.clone();
        let toc = run_blocking(move || db.get_book_toc(book_id, limit)).await?;
        self.respond(&snap, serde_json::to_value(toc).unwrap())
    }

    #[tool(
        name = "browse_categories",
        description = "Navigate the Otzaria category tree. With no category_id, returns the top-level categories. \
                       With a category_id, returns its child categories and the books directly under it. Use \
                       has_children to drill down and book_id with get_text / get_book_info.",
        annotations(read_only_hint = true)
    )]
    pub async fn browse_categories(
        &self,
        Parameters(p): Parameters<BrowseCategoriesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let snap = self.lib.current();
        let limit = p.limit.unwrap_or(50).min(200) as i64;
        let category_id = p.category_id;
        let db = snap.db.clone();
        let view = run_blocking(move || db.browse_categories(category_id, limit)).await?;
        self.respond(&snap, serde_json::to_value(view).unwrap())
    }

    /// Stamp a tool payload with snapshot identity (schema_version, data_version)
    /// and return it as pretty JSON text content.
    fn respond(
        &self,
        snap: &Snapshot,
        mut payload: serde_json::Value,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("schema_version".into(), json!(snap.db.schema_version()));
            obj.insert("data_version".into(), json!(snap.fingerprint));
        }
        let text = serde_json::to_string_pretty(&payload)
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
                "Offline access to the Otzaria Jewish-texts library (SQLite + Tantivy), read-only. \
                 Find a book_id with search_books / resolve_ref / browse_categories, then get_text to read \
                 lines, get_book_toc to navigate, get_links / get_commentaries for cross-references, or \
                 full_text_search across the whole library. All responses include schema_version and \
                 data_version (a snapshot fingerprint).",
            )
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Run a blocking DB/index call off the async runtime, mapping errors to MCP errors.
async fn run_blocking<T, F>(f: F) -> Result<T, ErrorData>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ErrorData::internal_error(format!("task join error: {e}"), None))?
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))
}
