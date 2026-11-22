//! Closing idle SQLite connections, so the MCP never holds Otzaria's databases while it is
//! not reading them.
//!
//! Otzaria updates `seforim.db` in place: it switches the database to WAL, applies the
//! patch, and switches back with `PRAGMA journal_mode=DELETE`. That last step needs every
//! other connection closed: an idle read-only connection to a WAL database still holds a
//! shared lock in its `-shm` file, so a reader that keeps its connection open makes the
//! step fail ("database is locked") and leaves the library in WAL mode. On Windows any
//! open handle also stops a database from being renamed or replaced (an attached database's
//! update swaps its file so).
//!
//! So every [`crate::db::Db`] opened from a file, and the search engine's own line-source
//! connection (through its public `refresh_line_source`, see `crate::search`), are closed
//! once they have been idle for [`IDLE`], by a background thread that looks every
//! [`TICK`]; and at once after each use while the database is in WAL mode or the app's
//! update marker (`<db>.applying`) is present. The next query reopens the file
//! (about a millisecond; the zstd dictionaries and the table probe are kept).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once, Weak};
use std::time::Duration;

use crate::db::ConnSlot;

/// How long an unused connection stays open.
pub const IDLE: Duration = Duration::from_millis(750);
/// How often the reaper looks.
pub const TICK: Duration = Duration::from_millis(200);

static SLOTS: Mutex<Vec<Weak<ConnSlot>>> = Mutex::new(Vec::new());
static REAPER: Once = Once::new();

/// Follow a connection (a `Db` opened from a file), starting the reaper on first use.
pub(crate) fn register(slot: &Arc<ConnSlot>) {
    SLOTS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(Arc::downgrade(slot));
    REAPER.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("otzaria-mcp-idle".into())
            .spawn(|| loop {
                std::thread::sleep(TICK);
                sweep(IDLE);
            });
        if let Err(e) = spawned {
            eprintln!("otzaria-mcp: cannot start the idle-connection reaper: {e}");
        }
    });
}

/// Close every connection idle for `idle` (or wanted by the app now); returns how many.
pub fn sweep(idle: Duration) -> usize {
    let slots: Vec<Arc<ConnSlot>> = {
        let mut all = SLOTS.lock().unwrap_or_else(|p| p.into_inner());
        all.retain(|w| w.strong_count() > 0);
        all.iter().filter_map(Weak::upgrade).collect()
    };
    let mut closed = 0;
    for slot in &slots {
        if slot.release_if_idle(idle) {
            closed += 1;
        }
    }
    closed + crate::search::release_idle_line_source(idle, &release_now)
}

/// Whether the app needs `db` released now: it is in WAL mode (the app switches to WAL
/// for an update and back when it is done), or the app's update marker is present.
pub fn release_now(db: &Path) -> bool {
    is_wal(db) || update_marker(db).exists()
}

/// `<db>.applying`, which Otzaria's library updater writes while it patches the database
/// (`otzaria_library_updater`'s `markerPathFor`).
pub fn update_marker(db: &Path) -> PathBuf {
    let mut marker = db.as_os_str().to_owned();
    marker.push(".applying");
    PathBuf::from(marker)
}

/// The header's file-format version bytes (18 and 19) are 2 for a WAL database.
pub fn is_wal(db: &Path) -> bool {
    let mut header = [0u8; 20];
    std::fs::File::open(db)
        .and_then(|mut f| f.read_exact(&mut header))
        .is_ok()
        && (header[18] == 2 || header[19] == 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    /// `sweep` acts on every connection of the process: one test at a time.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn make_db(dir: &Path, wal: bool) -> PathBuf {
        let path = dir.join("t.db");
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(&format!(
            "PRAGMA journal_mode={}; CREATE TABLE book(id INTEGER PRIMARY KEY, title TEXT);
             INSERT INTO book VALUES (1, 'x');",
            if wal { "WAL" } else { "DELETE" }
        ))
        .unwrap();
        path
    }

    fn title(db: &Db) -> String {
        db.lock()
            .conn
            .query_row("SELECT title FROM book WHERE id = 1", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn idle_connection_is_closed_and_reopened() {
        let _serial = serial();
        let tmp = tempfile::tempdir().unwrap();
        let path = make_db(tmp.path(), false);
        let db = Db::open_file(&path, false).unwrap();
        assert!(!is_wal(&path));
        assert_eq!(title(&db), "x");
        assert!(db.is_open());
        // used just now: kept
        sweep(Duration::from_secs(60));
        assert!(db.is_open());
        // idle: closed, and the next query reopens it
        std::thread::sleep(Duration::from_millis(20));
        sweep(Duration::from_millis(10));
        assert!(!db.is_open());
        assert_eq!(title(&db), "x");
        assert!(db.is_open());
    }

    #[test]
    fn a_held_connection_is_never_closed() {
        let _serial = serial();
        let tmp = tempfile::tempdir().unwrap();
        let path = make_db(tmp.path(), false);
        let db = Db::open_file(&path, false).unwrap();
        let guard = db.lock();
        std::thread::sleep(Duration::from_millis(20));
        sweep(Duration::ZERO);
        drop(guard);
        assert!(db.is_open());
    }

    #[test]
    fn wal_database_is_released_after_each_use() {
        let _serial = serial();
        let tmp = tempfile::tempdir().unwrap();
        let path = make_db(tmp.path(), true);
        assert!(is_wal(&path));
        let db = Db::open_file(&path, false).unwrap();
        assert_eq!(title(&db), "x");
        assert!(
            !db.is_open(),
            "a WAL database is closed when the query ends"
        );
        assert_eq!(title(&db), "x");
    }

    #[test]
    fn update_marker_releases_at_once() {
        let _serial = serial();
        let tmp = tempfile::tempdir().unwrap();
        let path = make_db(tmp.path(), false);
        let db = Db::open_file(&path, false).unwrap();
        assert_eq!(title(&db), "x");
        assert!(db.is_open());
        std::fs::write(update_marker(&path), b"").unwrap();
        sweep(Duration::from_secs(60));
        assert!(!db.is_open());
        assert_eq!(title(&db), "x");
        assert!(!db.is_open());
    }

    /// The bug this module fixes: the app's switch back to DELETE succeeds while the MCP
    /// has the database open, as soon as the MCP's connection is idle.
    #[test]
    fn app_can_leave_wal_mode_while_the_mcp_runs() {
        let _serial = serial();
        let tmp = tempfile::tempdir().unwrap();
        let path = make_db(tmp.path(), true);
        let db = Db::open_file(&path, false).unwrap();
        assert_eq!(title(&db), "x");
        let app = rusqlite::Connection::open(&path).unwrap();
        {
            // what happened before: a connection kept open blocks the switch
            let held = db.lock();
            let n: i64 = held
                .conn
                .query_row("SELECT count(*) FROM book", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 1);
            app.busy_timeout(Duration::ZERO).unwrap();
            let r = app.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get::<_, String>(0));
            assert!(
                r.map_or(true, |m| m == "wal"),
                "switched while a reader held it"
            );
            assert!(is_wal(&path));
        }
        app.busy_timeout(Duration::from_secs(5)).unwrap();
        app.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        let mode: String = app
            .query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
        assert!(!is_wal(&path));
        drop(app);
        assert_eq!(title(&db), "x");
    }

    #[test]
    fn in_memory_fixtures_are_left_alone() {
        let _serial = serial();
        let db = Db::from_connection(rusqlite::Connection::open_in_memory().unwrap()).unwrap();
        sweep(Duration::ZERO);
        assert!(db.is_open());
    }
}
