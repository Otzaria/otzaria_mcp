use std::path::Path;
use std::sync::Arc;

use otzaria_mcp::paths::Paths;
use otzaria_mcp::reload::{fingerprint, Library};
use otzaria_mcp::search::{Mode, SearchRequest};
use rusqlite::Connection;
use search_engine::api::search_engine::{refresh_line_source, SearchEngine, TextStorage};
use tokio_util::sync::CancellationToken;

// Every fixture configures the process-global engine line source. Serialize
// fixtures, but deliberately run many queries together inside the last test.
static SOURCE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn paths(root: &Path, indexed: bool) -> Paths {
    Paths {
        db: root.join("seforim.db"),
        index: indexed.then(|| root.join("index")),
        lexical_db: None,
        dictionaries_dir: None,
        snapshot: false,
    }
}

fn make_db(path: &Path, text: &str) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_meta(key TEXT PRIMARY KEY,value TEXT);
         INSERT INTO schema_meta VALUES('db_version','1'),('db_schema_version','5');
         CREATE TABLE book(id INTEGER PRIMARY KEY,title TEXT,totalLines INTEGER);
         CREATE TABLE line(id INTEGER PRIMARY KEY,bookId INTEGER,lineIndex INTEGER,heRef TEXT,content TEXT);
         INSERT INTO book VALUES(1,'Example',1);",
    ).unwrap();
    conn.execute("INSERT INTO line VALUES(1,1,0,'Example 1',?1)", [text])
        .unwrap();
}

fn make_index(db: &Path, path: &Path, text: &str) {
    refresh_line_source(db.to_string_lossy().into_owned()).unwrap();
    std::fs::create_dir(path).unwrap();
    let mut writer = SearchEngine::new(path.to_str().unwrap());
    writer.set_economy_indexing(true).unwrap();
    writer
        .add_text_book(
            "Example".into(),
            "/samples/id:1".into(),
            "id:1".into(),
            text.into(),
            None,
            TextStorage::LibraryDb,
            None,
            None,
        )
        .unwrap();
    writer.commit().unwrap();
}

#[tokio::test]
async fn wal_commit_and_checkpoint_are_observed_without_touching_main_db_first() {
    let _source = SOURCE.lock().await;
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path(), false);
    make_db(&paths.db, "old");
    let writer = Connection::open(&paths.db).unwrap();
    writer
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    let library = Library::open(paths.clone()).unwrap();
    let before = fingerprint(&paths);
    writer.execute_batch("BEGIN; UPDATE line SET content='new'; UPDATE schema_meta SET value='2' WHERE key='db_version'; COMMIT;").unwrap();
    assert_ne!(fingerprint(&paths), before);
    assert!(library.refresh().await.unwrap());
    assert_eq!(library.current().schema_version, Some(2));
    assert_eq!(
        library
            .current()
            .db
            .get_text(1, 0, 1, true, None)
            .unwrap()
            .unwrap()
            .lines[0]
            .content,
        "new"
    );
    let committed = fingerprint(&paths);
    writer
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    assert_ne!(fingerprint(&paths), committed);
    assert!(library.refresh().await.unwrap());
    assert!(!library.refresh().await.unwrap());
}

#[tokio::test]
async fn incompatible_or_missing_index_keeps_database_tools_available() {
    let _source = SOURCE.lock().await;
    let root = tempfile::tempdir().unwrap();
    let mut paths = paths(root.path(), true);
    make_db(&paths.db, "שלום דוגמה");
    let index = paths.index.as_ref().unwrap().clone();
    make_index(&paths.db, &index, "שלום דוגמה");
    let metadata_path = index.join("otzaria_index_meta.json");
    let original: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&metadata_path).unwrap()).unwrap();
    for version in [7, 8, 9, 99] {
        let mut metadata = original.clone();
        metadata["schema_version"] = version.into();
        let bytes = serde_json::to_vec(&metadata).unwrap();
        std::fs::write(&metadata_path, &bytes).unwrap();
        let library = Library::open(paths.clone()).unwrap();
        let current = library.current();
        assert!(current.search.is_none());
        let error = current.search_error.as_deref().unwrap();
        assert!(
            error.contains(if version == 99 {
                "engine_too_old"
            } else {
                "rebuild_required"
            }),
            "{error}"
        );
        assert_eq!(
            current
                .db
                .get_text(1, 0, 1, true, None)
                .unwrap()
                .unwrap()
                .lines[0]
                .content,
            "שלום דוגמה"
        );
        assert_eq!(
            std::fs::read(&metadata_path).unwrap(),
            bytes,
            "read-only open rewrote metadata"
        );
    }
    std::fs::remove_file(&metadata_path).unwrap();
    let library = Library::open(paths.clone()).unwrap();
    assert!(
        library.current().search.is_none(),
        "populated index without metadata must not bypass compatibility"
    );
    assert!(!metadata_path.exists());
    let missing = root.path().join("absent-index");
    paths.index = Some(missing.clone());
    let library = Library::open(paths).unwrap();
    assert!(library.current().search.is_none());
    assert!(
        !missing.exists(),
        "read-only open created an index directory"
    );
}

#[test]
fn empty_wal_and_sqlite_permission_touches_are_not_content_changes() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path(), false);
    make_db(&paths.db, "old");
    let before = fingerprint(&paths);
    let wal = root.path().join("seforim.db-wal");
    std::fs::write(&wal, []).unwrap();
    assert_eq!(fingerprint(&paths), before);
    std::fs::write(&wal, b"wal frame fixture").unwrap();
    let with_data = fingerprint(&paths);
    assert_ne!(with_data, before);
    let permissions = std::fs::metadata(&wal).unwrap().permissions();
    std::fs::set_permissions(&wal, permissions).unwrap();
    assert_eq!(fingerprint(&paths), with_data);
    std::fs::write(&wal, []).unwrap();
    assert_eq!(fingerprint(&paths), before);
}

#[test]
fn snapshot_mode_does_not_watch_wal_that_immutable_sqlite_ignores() {
    let root = tempfile::tempdir().unwrap();
    let mut paths = paths(root.path(), false);
    make_db(&paths.db, "old");
    paths.snapshot = true;
    let before = fingerprint(&paths);
    std::fs::write(
        root.path().join("seforim.db-wal"),
        b"ignored by immutable reader",
    )
    .unwrap();
    assert_eq!(fingerprint(&paths), before);
}

#[cfg(unix)]
#[test]
fn replacement_with_same_size_and_mtime_changes_identity() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path(), false);
    make_db(&paths.db, "old");
    let before = fingerprint(&paths);
    let times = std::fs::FileTimes::new()
        .set_modified(std::fs::metadata(&paths.db).unwrap().modified().unwrap());
    let replacement = root.path().join("replacement.db");
    make_db(&replacement, "new");
    assert_eq!(
        std::fs::metadata(&paths.db).unwrap().len(),
        std::fs::metadata(&replacement).unwrap().len()
    );
    std::fs::File::options()
        .write(true)
        .open(&replacement)
        .unwrap()
        .set_times(times)
        .unwrap();
    std::fs::rename(replacement, &paths.db).unwrap();
    assert_ne!(fingerprint(&paths), before);
}

#[tokio::test]
async fn changed_generation_while_waiting_is_discarded_then_current_generation_reopens() {
    let _source = SOURCE.lock().await;
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path(), false);
    make_db(&paths.db, "old");
    let library = Library::open(paths.clone()).unwrap();
    let writer = Connection::open(&paths.db).unwrap();
    writer
        .execute("UPDATE line SET content='first'", [])
        .unwrap();
    let held = library
        .search_admission()
        .acquire(CancellationToken::new())
        .await
        .unwrap();
    let waiting_library = library.clone();
    let pending = tokio::spawn(async move { waiting_library.refresh().await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while library.search_admission().metrics().in_flight < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    writer
        .execute("UPDATE line SET content='second'", [])
        .unwrap();
    drop(held);
    assert!(!pending.await.unwrap().unwrap());
    assert!(library.refresh().await.unwrap());
    assert_eq!(
        library
            .current()
            .db
            .get_text(1, 0, 1, true, None)
            .unwrap()
            .unwrap()
            .lines[0]
            .content,
        "second"
    );
}

#[tokio::test]
async fn concurrent_refreshes_of_one_generation_open_it_once() {
    let _source = SOURCE.lock().await;
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path(), false);
    make_db(&paths.db, "old");
    let library = Library::open(paths.clone()).unwrap();
    Connection::open(&paths.db)
        .unwrap()
        .execute("UPDATE line SET content='new'", [])
        .unwrap();
    let held = library
        .search_admission()
        .acquire(CancellationToken::new())
        .await
        .unwrap();
    let (started, mut received) = tokio::sync::mpsc::unbounded_channel();
    let mut tasks = vec![];
    for _ in 0..2 {
        let library = library.clone();
        let started = started.clone();
        tasks.push(tokio::spawn(async move {
            started.send(()).unwrap();
            library.refresh().await.unwrap()
        }));
    }
    // On this current-thread runtime, each task reaches the admission await
    // before the receiver can resume, so both observed the old snapshot.
    received.recv().await.unwrap();
    received.recv().await.unwrap();
    drop(held);
    let mut reopened = 0;
    for task in tasks {
        reopened += usize::from(task.await.unwrap());
    }
    assert_eq!(
        reopened, 1,
        "the second queued refresh needlessly reopened the same files"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_searches_and_atomic_db_index_replacement_publish_fresh_handles() {
    let _source = SOURCE.lock().await;
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path(), true);
    make_db(&paths.db, "שלום קודם");
    make_index(&paths.db, paths.index.as_ref().unwrap(), "שלום קודם");
    let library = Library::open(paths.clone()).unwrap();
    let old = library.current();
    let request = SearchRequest::new("שלום", Mode::Exact);
    assert!(
        old.search.as_ref().unwrap().search(&request).unwrap().hits[0]
            .snippet
            .contains("קודם")
    );

    let replacement_db = root.path().join("replacement.db");
    let replacement_index = root.path().join("replacement-index");
    make_db(&replacement_db, "שלום חדש");
    // Construct the replacement before the atomic publication. A real app is a
    // separate process; restore the MCP source path before starting its requests.
    make_index(&replacement_db, &replacement_index, "שלום חדש");
    refresh_line_source(paths.db.to_string_lossy().into_owned()).unwrap();
    let held = library
        .search_admission()
        .acquire(CancellationToken::new())
        .await
        .unwrap();
    let mut requests = vec![];
    for _ in 0..16 {
        let library = Arc::clone(&library);
        requests.push(tokio::spawn(async move {
            let admitted = library
                .search_admission()
                .acquire(CancellationToken::new())
                .await
                .unwrap();
            admitted
                .run(move || {
                    let snapshot = library.current();
                    snapshot
                        .search
                        .as_ref()
                        .unwrap()
                        .search(&SearchRequest::new("שלום", Mode::Exact))
                        .unwrap()
                })
                .await
                .unwrap()
        }));
    }
    std::fs::rename(&replacement_db, &paths.db).unwrap();
    let active_index = paths.index.as_ref().unwrap();
    std::fs::rename(active_index, root.path().join("retired-index")).unwrap();
    std::fs::rename(replacement_index, active_index).unwrap();
    let reloading = Arc::clone(&library);
    let refresh = tokio::spawn(async move { reloading.refresh().await });
    drop(held);
    for result in requests {
        let response = result.await.unwrap();
        assert_eq!(response.total, 1);
        let hit = &response.hits[0];
        assert!(matches!(hit.text_status, "ok" | "stale"));
        if hit.text_status == "stale" {
            assert!(!hit.snippet.contains('«'));
        }
    }
    assert!(refresh.await.unwrap().unwrap());
    let current = library.current();
    assert!(!Arc::ptr_eq(&old, &current));
    let response = current.search.as_ref().unwrap().search(&request).unwrap();
    assert_eq!(response.hits[0].text_status, "ok");
    assert!(response.hits[0].snippet.contains("חדש"));
    assert_eq!(
        current
            .db
            .get_text(1, 0, 1, true, None)
            .unwrap()
            .unwrap()
            .lines[0]
            .content,
        "שלום חדש"
    );
    // Retired handles may still be held by a caller, but must never claim that
    // the new live text is the text indexed in the old generation.
    let retired = old.search.as_ref().unwrap().search(&request).unwrap();
    assert_eq!(retired.hits[0].text_status, "stale");
    assert!(!retired.hits[0].snippet.contains('«'));
}
