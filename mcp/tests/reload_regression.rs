use otzaria_mcp::search::{Mode, Search, SearchRequest};
use rusqlite::Connection;
use search_engine::api::search_engine::{
    configure_line_source, line_source_status, refresh_line_source, BookOrderEntry, SearchEngine,
    TextStorage,
};
use std::sync::Mutex;

#[cfg(unix)]
use otzaria_mcp::db::Db;
#[cfg(unix)]
use otzaria_mcp::paths::Paths;
#[cfg(unix)]
use std::sync::Arc;

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn make_db(path: &std::path::Path, text: &str) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE book (id INTEGER PRIMARY KEY, title TEXT, totalLines INTEGER);
         CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, heRef TEXT, content TEXT);
         INSERT INTO book VALUES (1, 'ספר', 1);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO line (id, bookId, lineIndex, content) VALUES (1, 1, 0, ?1)",
        [text],
    )
    .unwrap();
}

fn make_index(db: &std::path::Path, index_dir: &std::path::Path) {
    configure_line_source(db.to_string_lossy().into_owned()).unwrap();
    std::fs::create_dir_all(index_dir).unwrap();
    let mut writer = SearchEngine::new(index_dir.to_str().unwrap());
    add_book(&mut writer, 1, "שלום עולם");
    writer.commit().unwrap();
}

fn add_book(writer: &mut SearchEngine, book_id: i64, text: &str) {
    writer
        .add_text_book(
            format!("ספר {book_id}"),
            format!("/ספר/id:{book_id}"),
            format!("id:{book_id}"),
            text.into(),
            None,
            TextStorage::LibraryDb,
            None,
            None,
        )
        .unwrap();
}

fn place(book_id: i64, catalogue_order: u32, generation_order: u32) -> BookOrderEntry {
    BookOrderEntry {
        file_path: format!("id:{book_id}"),
        catalogue_order,
        generation_order,
    }
}

fn book_ids(search: &Search, order: &str) -> Vec<i64> {
    let mut request = SearchRequest::new("שלום", Mode::Exact);
    request.order = order.into();
    let response = search.search(&request).unwrap();
    response.hits.iter().map(|h| h.book_id.unwrap()).collect()
}

#[test]
fn catalogue_and_generation_order_follow_the_app_mapping() {
    let _guard = TEST_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("seforim.db");
    let conn_text = ["שלום א", "שלום ב", "שלום ג"];
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE book (id INTEGER PRIMARY KEY, title TEXT, totalLines INTEGER);
         CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, heRef TEXT, content TEXT);",
    )
    .unwrap();
    for (i, text) in conn_text.iter().enumerate() {
        let id = i as i64 + 1;
        conn.execute("INSERT INTO book VALUES (?1, 'ספר', 1)", [id])
            .unwrap();
        conn.execute(
            "INSERT INTO line (id, bookId, lineIndex, content) VALUES (?1, ?1, 0, ?2)",
            rusqlite::params![id, text],
        )
        .unwrap();
    }
    drop(conn);
    configure_line_source(db.to_string_lossy().into_owned()).unwrap();
    let index_dir = tmp.path().join("index");
    std::fs::create_dir_all(&index_dir).unwrap();
    let mut writer = SearchEngine::new(index_dir.to_str().unwrap());
    // Book numbers follow the write order: 1, 2, 3.
    for (i, text) in conn_text.iter().enumerate() {
        add_book(&mut writer, i as i64 + 1, text);
    }
    writer.commit().unwrap();

    // No mapping file yet: every book sorts by its number.
    let search = Search::open(&index_dir, None, &db, None).unwrap();
    assert_eq!(book_ids(&search, "catalogue"), vec![1, 2, 3]);

    // The app (another engine) writes the mapping; the read-only engine takes it in.
    writer
        .set_book_order(vec![place(3, 0, 1), place(1, 1, 0)])
        .unwrap();
    assert_eq!(book_ids(&search, "catalogue"), vec![3, 1, 2]);
    assert_eq!(book_ids(&search, "generation"), vec![1, 3, 2]);

    // A fresh read-only open reads the mapping from the index directory.
    drop(search);
    let reopened = Search::open(&index_dir, None, &db, None).unwrap();
    assert_eq!(book_ids(&reopened, "catalogue"), vec![3, 1, 2]);
}

#[test]
fn refresh_same_path_closes_and_reopens_line_source() {
    let _guard = TEST_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("seforim.db");
    make_db(&db, "שלום עולם");
    let index_dir = tmp.path().join("index");
    make_index(&db, &index_dir);

    let search = Search::open(&index_dir, None, &db, None).unwrap();
    let request = SearchRequest::new("שלום", Mode::Exact);
    let initial = search.search(&request).unwrap();
    assert!(initial.hits[0].snippet.contains("עולם"), "{initial:?}");
    assert!(line_source_status().open);

    refresh_line_source(db.to_string_lossy().into_owned()).unwrap();
    assert!(!line_source_status().open);

    let refreshed = search.search(&request).unwrap();
    assert!(refreshed.hits[0].snippet.contains("עולם"), "{refreshed:?}");
    assert_eq!(refreshed.hits[0].text_status, "ok", "{refreshed:?}");
    assert!(line_source_status().open);
}

#[cfg(unix)]
#[test]
fn reopen_after_atomic_database_replacement_reads_new_text() {
    let _guard = TEST_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("seforim.db");
    make_db(&db, "שלום עולם");
    let index_dir = tmp.path().join("index");
    make_index(&db, &index_dir);

    let first = Arc::new(Search::open(&index_dir, None, &db, None).unwrap());
    let request = SearchRequest::new("שלום", Mode::Exact);
    let old = first.search(&request).unwrap();
    assert!(old.hits[0].snippet.contains("עולם"), "{old:?}");

    // Keep searches in flight while Search::open refreshes the process-wide line
    // source. The source mutex must serialize refresh against active row windows.
    let searching = Arc::clone(&first);
    let worker_request = request.clone();
    let worker = std::thread::spawn(move || {
        for _ in 0..20 {
            searching.search(&worker_request).unwrap();
        }
    });

    let replacement = tmp.path().join("replacement.db");
    make_db(&replacement, "שלום חדש");
    std::fs::rename(&replacement, &db).unwrap();

    let reopened = Search::open(&index_dir, None, &db, None).unwrap();
    let fresh = reopened.search(&request).unwrap();
    assert_eq!(fresh.hits.len(), 1, "{fresh:?}");
    assert!(fresh.hits[0].snippet.contains("חדש"), "{fresh:?}");
    assert_eq!(fresh.hits[0].text_status, "stale", "{fresh:?}");

    let db = Db::open(&Paths {
        db: db.clone(),
        index: Some(index_dir),
        lexical_db: None,
        dictionaries_dir: None,
        snapshot: false,
    })
    .unwrap();
    let text = db.get_text(1, 0, 1, false, None).unwrap().unwrap();
    assert_eq!(text.lines[0].content, "שלום חדש");

    worker.join().unwrap();
}
