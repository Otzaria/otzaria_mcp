use otzaria_mcp::db::Db;
use otzaria_mcp::search::{Mode, Search, SearchRequest};
use rusqlite::Connection;
use search_engine::api::library_indexing::{
    IndexingCancellationToken, LibraryBookInput, LibraryBookSource,
};
use search_engine::api::search_engine::configure_line_source;
use search_engine::api::search_engine::{SearchEngine, TextStorage};
use std::sync::Mutex;

static SOURCE: Mutex<()> = Mutex::new(());

fn input() -> LibraryBookInput {
    LibraryBookInput {
        source: LibraryBookSource::Library,
        attached_slug: None,
        book_id: 1,
        db_title: Some("ספר".into()),
        title: "ספר".into(),
        topics: "/ספר/id:1".into(),
        file_path: "id:1".into(),
        extra_facets: None,
        source_hash: None,
        text_storage: TextStorage::LibraryDb,
    }
}

#[test]
fn get_text_retains_schema7_footnote_body_found_by_search() {
    let _guard = SOURCE.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("seforim.db");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        r#"CREATE TABLE book (id INTEGER PRIMARY KEY, title TEXT, totalLines INTEGER);
         CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, heRef TEXT);
         CREATE TABLE line_content (id INTEGER PRIMARY KEY, content TEXT);
         CREATE TABLE line_note (lineId INTEGER, noteIndex INTEGER, charPos INTEGER, marker TEXT, body TEXT);
         INSERT INTO book VALUES (1, 'ספר', 1);
         INSERT INTO line VALUES (1, 1, 0, 'ספר א');
         INSERT INTO line_content VALUES (1, 'שלום<sup class="footnote-marker">א</sup> עולם');
         INSERT INTO line_note VALUES (1, 0, 5, 'א', 'הערה מילתבדיקה הסבר');"#,
    ).unwrap();
    let index_path = tmp.path().join("index");
    std::fs::create_dir(&index_path).unwrap();
    configure_line_source(db_path.to_string_lossy().into_owned()).unwrap();
    let mut writer = SearchEngine::new(index_path.to_str().unwrap());
    writer
        .add_library_books(vec![input()], &IndexingCancellationToken::new())
        .unwrap();
    writer.commit().unwrap();
    let search = Search::open(&index_path, None, &db_path, None).unwrap();
    let hits = search
        .search(&SearchRequest::new("מילתבדיקה", Mode::Exact))
        .unwrap();
    assert_eq!(hits.total, 1);
    assert!(hits.hits[0].snippet.contains("מילתבדיקה"));
    let db = Db::from_connection(conn).unwrap();
    let text = db.get_text(1, 0, 1, true, None).unwrap().unwrap();
    assert!(
        text.lines[0].content.contains("מילתבדיקה"),
        "get_text lost the body: {:?}",
        text.lines[0].content
    );
}
