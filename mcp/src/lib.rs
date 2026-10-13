//! otzaria-mcp library crate: read-only access to the Otzaria library
//! (SQLite `seforim.db` + Tantivy index) exposed as MCP tools. The `main.rs`
//! binary is a thin wrapper; these modules are also used by the integration tests.

pub mod db;
pub mod links;
pub mod paths;
pub mod refkey;
pub mod refs;
pub mod reload;
pub mod search;
pub mod server;
pub mod text;
