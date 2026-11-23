use otzaria_mcp::{db::Db, idle};
use rusqlite::Connection;
use std::{fs, path::Path, sync::Mutex, time::Duration};

static SERIAL: Mutex<()> = Mutex::new(());

fn fixture(path: &Path, split: bool) {
    let connection = Connection::open(path).unwrap();
    connection.execute_batch(
        "CREATE TABLE book(id INTEGER PRIMARY KEY, title TEXT NOT NULL, totalLines INTEGER NOT NULL,
            heRef TEXT, isBaseBook INTEGER NOT NULL, orderIndex INTEGER NOT NULL);
         INSERT INTO book VALUES(1, 'Fixture', 1, 'Fixture', 1, 1);",
    ).unwrap();
    if split {
        connection.execute_batch(
            "CREATE TABLE line(id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, heRef TEXT);
             CREATE TABLE line_content(id INTEGER PRIMARY KEY, content TEXT);
             INSERT INTO line VALUES(1, 1, 0, 'Fixture 1');
             INSERT INTO line_content VALUES(1, 'new text');",
        ).unwrap();
    } else {
        connection.execute_batch(
            "CREATE TABLE line(id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, heRef TEXT, content TEXT);
             INSERT INTO line VALUES(1, 1, 0, 'Fixture 1', 'old text');",
        ).unwrap();
    }
}

#[test]
fn idle_reopen_cannot_mix_replacement_and_old_schema() {
    let _serial = SERIAL.lock().unwrap_or_else(|guard| guard.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("seforim.db");
    fixture(&path, false);
    let db = Db::open_file(&path, false).unwrap();
    assert_eq!(
        db.get_text(1, 0, 1, false, None).unwrap().unwrap().lines[0].content,
        "old text"
    );
    idle::sweep(Duration::ZERO);
    assert!(!db.is_open());
    fs::rename(&path, dir.path().join("old.db")).unwrap();
    fixture(&path, true);
    assert!(db.get_text(1, 0, 1, false, None).is_err());
    let db = Db::open_file(&path, false).unwrap();
    assert_eq!(
        db.get_text(1, 0, 1, false, None).unwrap().unwrap().lines[0].content,
        "new text"
    );
}

#[test]
fn temporary_reopen_failure_does_not_cache_empty_title_index() {
    let _serial = SERIAL.lock().unwrap_or_else(|guard| guard.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("seforim.db");
    let moved = dir.path().join("moved.db");
    fixture(&path, false);
    let db = Db::open_file(&path, false).unwrap();
    idle::sweep(Duration::ZERO);
    assert!(!db.is_open());
    fs::rename(&path, &moved).unwrap();
    assert!(db.lookup_title("Fixture", 10).is_err());
    fs::rename(&moved, &path).unwrap();
    let after = db.lookup_title("Fixture", 10).unwrap();
    assert_eq!(
        db.get_text(1, 0, 1, false, None).unwrap().unwrap().lines[0].content,
        "old text"
    );
    assert_eq!(after.len(), 1);
}

#[test]
fn committed_wal_update_invalidates_a_warm_title_cache() {
    let _serial = SERIAL.lock().unwrap_or_else(|guard| guard.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("seforim.db");
    fixture(&path, false);
    let writer = Connection::open(&path).unwrap();
    writer.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    let db = Db::open_file(&path, false).unwrap();
    assert_eq!(db.lookup_title("Fixture", 10).unwrap().len(), 1);
    writer
        .execute("UPDATE book SET title='Updated', heRef='Updated'", [])
        .unwrap();
    assert!(db.lookup_title("Fixture", 10).unwrap().is_empty());
    assert_eq!(db.lookup_title("Updated", 10).unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
fn wal_detection_preserves_other_connections_read_locks() {
    use std::{env, process::Command};

    const WRITER_TEST_PATH: &str = "OTZARIA_REVIEW_WRITER_TEST_PATH";
    if let Some(path) = env::var_os(WRITER_TEST_PATH) {
        let writer = Connection::open(path).unwrap();
        writer.busy_timeout(Duration::ZERO).unwrap();
        let lock = writer.execute_batch("BEGIN EXCLUSIVE;");
        if lock.is_ok() {
            writer.execute_batch("ROLLBACK;").unwrap();
        }
        println!("WRITE_LOCK_ACQUIRED={}", lock.is_ok());
        return;
    }

    let _serial = SERIAL.lock().unwrap_or_else(|guard| guard.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("seforim.db");
    fixture(&path, false);
    let reader =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    reader.execute_batch("BEGIN;").unwrap();
    let count: i64 = reader
        .query_row("SELECT COUNT(*) FROM book", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);

    let separate_writer_acquires_lock = || {
        let writer = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "wal_detection_preserves_other_connections_read_locks",
                "--nocapture",
            ])
            .env(WRITER_TEST_PATH, &path)
            .output()
            .unwrap();
        assert!(writer.status.success(), "{writer:?}");
        let stdout = String::from_utf8(writer.stdout).unwrap();
        assert!(stdout.contains("WRITE_LOCK_ACQUIRED="), "{stdout}");
        stdout.contains("WRITE_LOCK_ACQUIRED=true")
    };
    assert!(
        !separate_writer_acquires_lock(),
        "reader should exclude writer before WAL detection"
    );
    assert!(!idle::release_now(&path));
    assert!(!reader.is_autocommit());
    assert!(
        !separate_writer_acquires_lock(),
        "reader should still exclude writer after WAL detection"
    );
}
