use std::collections::BTreeMap;
use std::path::Path;

use otzaria_mcp::search::{Mode, Search, SearchRequest};
use serde_json::{json, Value};

fn files(path: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

#[test]
fn mcp_reads_independently_built_schema10_index_without_changing_it() {
    let root = tempfile::tempdir().unwrap();
    let index = root.path().join("index");
    std::fs::create_dir(&index).unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/canonical-7.0.1/index");
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), index.join(entry.file_name())).unwrap();
    }
    let before = files(&index);
    let db = root.path().join("seforim.db");
    rusqlite::Connection::open(&db).unwrap().execute_batch(
        "CREATE TABLE book(id INTEGER PRIMARY KEY, title TEXT, totalLines INTEGER);
         CREATE TABLE line(id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, heRef TEXT, content TEXT);",
    ).unwrap();
    let search = Search::open(&index, None, &db, None).unwrap();
    assert_eq!(search.info.found_schema_version, Some(10));
    let mut pages = vec![];
    for ordering in ["catalogue", "generation", "relevance"] {
        for kind in ["exact", "phrase", "fuzzy", "grouped"] {
            let mut req = SearchRequest::new(
                match kind {
                    "phrase" => "מחשב מהיר",
                    "fuzzy" => "מחשף",
                    _ => "מחשב",
                },
                if kind == "fuzzy" {
                    Mode::Fuzzy
                } else {
                    Mode::Exact
                },
            );
            req.order = ordering.into();
            if kind == "fuzzy" {
                req.distance = Some(1);
            }
            if kind == "grouped" {
                req.grouping = "identical_text".into();
            }
            pages.push(json!({"case": format!("{kind}-{ordering}"), "response": search.search(&req).unwrap()}));
        }
    }
    let mut page = SearchRequest::new("מחשב", Mode::Exact);
    page.limit = 2;
    page.offset = 1;
    let mut facet = SearchRequest::new("מחשב", Mode::Exact);
    facet.facets = vec!["/devices".into()];
    let mut vocalized = SearchRequest::new("מַחְשֵׁב", Mode::Exact);
    vocalized.match_nikud = true;
    let mut negative = SearchRequest::new("מסך", Mode::Advanced);
    negative.negative_query = Some("ירוק".into());
    negative.scope = "same_paragraph".into();
    for (name, req) in [
        ("pagination", page),
        ("facet", facet),
        ("vocalized", vocalized),
        ("advanced-negative", negative),
    ] {
        pages.push(json!({"case": name, "response": search.search(&req).unwrap()}));
    }
    let actual = json!({
        "document_count": search.doc_count(), "pages": pages,
        "book_counts": search.count_by_book(&SearchRequest::new("מחשב", Mode::Exact)).unwrap(),
    });
    let expected: Value =
        serde_json::from_str(include_str!("fixtures/canonical-7.0.1/expected-mcp.json")).unwrap();
    assert_eq!(actual, expected);
    drop(search);
    assert_eq!(files(&index), before, "MCP modified an existing index");
    // After reader shutdown, hosts can replace the directory even on Windows.
    std::fs::rename(&index, root.path().join("retired")).unwrap();
}
