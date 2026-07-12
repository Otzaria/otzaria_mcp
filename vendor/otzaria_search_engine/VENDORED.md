# Vendored: otzaria_search_engine (rust crate `search_engine`)

This directory holds a **vendored copy** of the Rust crate from the
`otzaria_search_engine` Flutter package, pub.dev version **0.6.7**.

## Why vendored, and why this exact version
- The crate is not published to crates.io, and the upstream GitHub repo
  (`github.com/Otzaria/otzaria_search_engine`) has no release tags — its default
  branches are either older (schema v1) or newer (a refactor with a changed
  Tantivy schema). Neither can open the index the installed app actually built.
- The installed Otzaria app depends on `otzaria_search_engine: ^0.6.7`, and the
  on-disk Tantivy index carries `schema_version: 2`, `tantivy_version: 0.26.1`.
  The 0.6.7 crate's `current_schema()` matches that index exactly, so
  `SearchEngine::new` opens it without a schema-mismatch panic.
- Vendoring the exact 0.6.7 bytes makes the MCP self-contained and reproducible
  for anyone building it, without depending on an elusive upstream commit.

## Source
Copied from `~/.pub-cache/hosted/pub.dev/otzaria_search_engine-0.6.7/rust`
(only the `rust/` crate is needed; the Flutter plugin scaffolding is omitted).

## Upgrading
If a future Otzaria release rebuilds the index with a newer schema, re-vendor the
matching `otzaria_search_engine` version here and rebuild. Confirm compatibility
by comparing the crate's `INDEX_SCHEMA_VERSION` / `current_schema()` against the
index's `otzaria_index_meta.json` and `meta.json`.
