use std::path::{Path, PathBuf};

use anyhow::Result;
use otzaria_mcp::search::{Mode, Search, SearchRequest};
use rusqlite::{params, Connection};
use search_engine::api::search_engine::{
    configure_line_source, BookOrderEntry, SearchEngine, TextStorage,
};
use serde_json::{json, Value};

pub struct Fixture {
    pub root: tempfile::TempDir,
    pub db: PathBuf,
    pub index: PathBuf,
}

impl Fixture {
    pub fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        let db = root.path().join("seforim.db");
        let index = root.path().join("index");
        build_fixture(&db, &index)?;
        Ok(Self { root, db, index })
    }

    pub fn search(&self) -> Result<Search> {
        Search::open(&self.index, None, &self.db, None)
    }
}

/// Deliberately use sparse SQLite IDs and a different index insertion/catalogue
/// order: an internal engine book number must never escape as an MCP book ID.
pub fn build_fixture(db: &Path, index: &Path) -> Result<()> {
    let books = [
        (
            42_i64,
            "דוגמה ראשונה",
            "/samples/a",
            vec![
                "<h1>כותרת ראשונה</h1>",
                "שלום עולם",
                "שלום שלום שלום שלום קובץ",
                "אבן דרך",
                "שָׁלוֹם בָּהִיר למרחוק מאוד",
                "סוף חלון",
                "חדש בחיפוש",
            ],
        ),
        (
            7,
            "דוגמה שנייה",
            "/samples/b",
            vec![
                "<h1>כותרת אחרת</h1>",
                "שלום קובץ נוסף למבחן ארוך במיוחד",
                "דרך ארוכה",
                "בדיקה חוזרת",
                "בדרך קצרה",
            ],
        ),
        (
            93,
            "דוגמה שלישית",
            "/samples/a",
            vec!["<h1>כותרת שלישית</h1>", "שלום שלום קובץ", "בדיקה חוזרת"],
        ),
    ];
    let conn = Connection::open(db)?;
    conn.execute_batch(
        "CREATE TABLE book(id INTEGER PRIMARY KEY, title TEXT, totalLines INTEGER);
         CREATE TABLE line(id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, heRef TEXT, content TEXT);
         CREATE TABLE schema_meta(key TEXT PRIMARY KEY, value TEXT);
         INSERT INTO schema_meta VALUES ('db_version','1'),('db_schema_version','5');",
    )?;
    for (id, title, _, lines) in &books {
        conn.execute(
            "INSERT INTO book VALUES (?1, ?2, ?3)",
            params![id, title, lines.len()],
        )?;
        for (line_index, line) in lines.iter().enumerate() {
            conn.execute(
                "INSERT INTO line(bookId,lineIndex,heRef,content) VALUES (?1,?2,?3,?4)",
                params![id, line_index, format!("{title} {line_index}"), line],
            )?;
        }
    }
    drop(conn);
    configure_line_source(db.to_string_lossy().into_owned())?;
    std::fs::create_dir(index)?;
    let mut writer = SearchEngine::new(index.to_str().unwrap());
    for (id, title, category, lines) in books {
        writer.add_text_book(
            title.into(),
            format!("{category}/id:{id}"),
            format!("id:{id}"),
            lines.join("\n"),
            None,
            TextStorage::LibraryDb,
            None,
            None,
        )?;
    }
    writer.commit()?;
    writer.set_book_order(vec![
        BookOrderEntry {
            file_path: "id:93".into(),
            catalogue_order: 0,
            generation_order: 1,
        },
        BookOrderEntry {
            file_path: "id:42".into(),
            catalogue_order: 1,
            generation_order: 2,
        },
        BookOrderEntry {
            file_path: "id:7".into(),
            catalogue_order: 2,
            generation_order: 0,
        },
    ])?;
    drop(writer);
    Ok(())
}

pub fn requests() -> Vec<(&'static str, SearchRequest)> {
    let mut out = vec![];
    let exact = SearchRequest::new("שלום", Mode::Exact);
    out.push(("exact_catalogue", exact.clone()));
    let mut generation = exact.clone();
    generation.order = "generation".into();
    out.push(("exact_generation", generation));
    let mut relevance = exact.clone();
    relevance.order = "relevance".into();
    out.push(("exact_relevance", relevance));
    let mut page = exact.clone();
    page.limit = 2;
    page.offset = 1;
    out.push(("exact_page", page));
    let mut facet = exact.clone();
    facet.facets = vec!["/samples/a".into()];
    out.push(("facet_filter", facet));
    out.push(("phrase", SearchRequest::new("שלום עולם", Mode::Exact)));
    out.push(("cross_line", SearchRequest::new("חלון חדש", Mode::Exact)));
    out.push((
        "no_hits",
        SearchRequest::new("מחרוזתשאינהקיימת", Mode::Exact),
    ));
    let mut fuzzy = SearchRequest::new("שלופ", Mode::Fuzzy);
    fuzzy.distance = Some(1);
    out.push(("fuzzy_typo", fuzzy));
    out.push(("fuzzy_exact", SearchRequest::new("שלום", Mode::Fuzzy)));
    let mut advanced = SearchRequest::new("שלום", Mode::Advanced);
    advanced.negative_query = Some("עולם".into());
    out.push(("advanced_negative", advanced));
    let mut prefixes = SearchRequest::new("דרך", Mode::Exact);
    prefixes.options.prefixes = true;
    out.push(("advanced_prefixes", prefixes));
    let mut spacing = SearchRequest::new("שלום קובץ", Mode::Advanced);
    spacing.distance = Some(3);
    out.push(("advanced_distance", spacing));
    let mut some = SearchRequest::new("ארוכה בהיר", Mode::Advanced);
    some.word_match = "any".into();
    out.push(("advanced_any_word", some));
    let mut nikud = SearchRequest::new("שָׁלוֹם", Mode::Exact);
    nikud.match_nikud = true;
    out.push(("exact_nikud", nikud));
    let mut grouped = SearchRequest::new("בדיקה חוזרת", Mode::Exact);
    grouped.grouping = "identical_text".into();
    out.push(("grouped_identical", grouped));
    let mut section = exact.clone();
    section.grouping = "same_section".into();
    out.push(("grouped_section", section));
    out
}

/// JSON contract deliberately includes all hit fields, order, counts, snippets
/// and highlights. No sorting of hits or ignored output fields hides a change.
pub fn capture(search: &Search) -> Result<Value> {
    let mut scenarios = serde_json::Map::new();
    for (name, request) in requests() {
        let response = search.search(&request)?;
        let by_book = search.count_by_book(&request)?;
        let facets: Vec<_> = search
            .facet_counts(&request, "/samples")?
            .into_iter()
            .map(|f| json!({"path": f.path, "count": f.count}))
            .collect();
        scenarios.insert(
            name.into(),
            json!({
                "response": response, "book_counts": by_book, "facet_counts": facets,
            }),
        );
    }
    Ok(Value::Object(scenarios))
}

pub fn validate_contract(value: &Value) {
    let exact = &value["exact_catalogue"]["response"];
    assert!(exact["total"].as_u64().unwrap() >= 5, "{exact}");
    assert_eq!(exact["hits"][0]["book_id"], 93);
    assert_eq!(
        value["exact_generation"]["response"]["hits"][0]["book_id"],
        7
    );
    assert_eq!(value["no_hits"]["response"]["total"], 0);
    assert!(value["fuzzy_typo"]["response"]["total"].as_u64().unwrap() > 0);
    assert_eq!(value["advanced_prefixes"]["response"]["mode"], "advanced");
    assert!(exact["hits"].as_array().unwrap().iter().all(|hit| {
        matches!(hit["book_id"].as_i64(), Some(7 | 42 | 93))
            && hit["text_status"] == "ok"
            && hit["snippet"].as_str().unwrap().contains('«')
    }));
    let page = &value["exact_page"]["response"]["hits"];
    assert_eq!(
        page.as_array().unwrap(),
        &exact["hits"].as_array().unwrap()[1..3]
    );
}
