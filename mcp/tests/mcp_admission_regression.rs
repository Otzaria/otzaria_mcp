use std::time::Duration;

use otzaria_mcp::{paths::Paths, reload::Library, server::OtzariaServer};
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio_util::sync::CancellationToken;

type Reader = BufReader<ReadHalf<tokio::io::DuplexStream>>;
type Writer = WriteHalf<tokio::io::DuplexStream>;

async fn request(
    reader: &mut Reader,
    writer: &mut Writer,
    id: u32,
    method: &str,
    params: Value,
) -> Value {
    let body = json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params});
    writer
        .write_all(format!("{body}\n").as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    let value: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["id"], id, "{value}");
    value
}

async fn client(lib: std::sync::Arc<Library>) -> (Reader, Writer) {
    let (server, client) = tokio::io::duplex(16384);
    tokio::spawn(async move {
        OtzariaServer::new(lib)
            .serve(server)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
    });
    let (read, mut write) = tokio::io::split(client);
    let mut read = BufReader::new(read);
    let response = request(&mut read, &mut write, 1, "initialize", json!({"protocolVersion":"2025-03-26", "capabilities":{}, "clientInfo":{"name":"test", "version":"1"}})).await;
    assert!(response.get("result").is_some(), "{response}");
    write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    (read, write)
}

fn payload(response: &Value) -> Value {
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn library(path: std::path::PathBuf) -> std::sync::Arc<Library> {
    Library::open(Paths {
        db: path,
        index: None,
        lexical_db: None,
        dictionaries_dir: None,
        snapshot: false,
    })
    .unwrap()
}

#[tokio::test]
async fn mcp_responses_track_wal_metadata_without_reloading() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("test.db");
    let writer = rusqlite::Connection::open(&path).unwrap();
    writer.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT); INSERT INTO schema_meta VALUES ('db_version','32'),('db_schema_version','6'); CREATE TABLE book(id INTEGER PRIMARY KEY, title TEXT, totalLines INTEGER); CREATE TABLE line(id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, heRef TEXT, content TEXT); INSERT INTO book VALUES(1,'book',1); INSERT INTO line VALUES(1,1,0,'ref','old'); PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    let lib = library(path.clone());
    let original_fingerprint = lib.current().fingerprint.clone();
    let (mut reader, mut client) = client(lib).await;
    let first = request(
        &mut reader,
        &mut client,
        2,
        "tools/call",
        json!({"name":"get_text", "arguments":{"book_id":1}}),
    )
    .await;
    assert_eq!(payload(&first)["schema_version"], 32);
    writer.execute_batch("BEGIN; UPDATE schema_meta SET value='33' WHERE key='db_version'; UPDATE schema_meta SET value='7' WHERE key='db_schema_version'; UPDATE line SET content='new'; COMMIT;").unwrap();
    assert_eq!(
        original_fingerprint,
        otzaria_mcp::reload::fingerprint(&Paths {
            db: path,
            index: None,
            lexical_db: None,
            dictionaries_dir: None,
            snapshot: false
        })
    );
    let second = request(
        &mut reader,
        &mut client,
        3,
        "tools/call",
        json!({"name":"get_text", "arguments":{"book_id":1}}),
    )
    .await;
    let second = payload(&second);
    assert_eq!(second["schema_version"], 33);
    assert_eq!(second["lines"][0]["content"], "new");
    let info = request(
        &mut reader,
        &mut client,
        4,
        "tools/call",
        json!({"name":"library_info", "arguments":{}}),
    )
    .await;
    let info = payload(&info);
    assert_eq!(info["schema_version"], 33);
    assert_eq!(info["db_version"], 33);
    assert_eq!(info["db_schema_version"], 7);
}

#[tokio::test]
async fn invalid_search_parameters_win_over_full_queue() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("test.db");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE book(id INTEGER PRIMARY KEY, title TEXT, totalLines INTEGER);")
        .unwrap();
    let lib = library(path);
    let held = lib
        .search_admission()
        .acquire(CancellationToken::new())
        .await
        .unwrap();
    let mut waiters = Vec::new();
    for _ in 0..32 {
        let lib = lib.clone();
        waiters.push(tokio::spawn(async move {
            lib.search_admission()
                .acquire(CancellationToken::new())
                .await
        }));
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while lib.search_admission().metrics().in_flight != 33 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (mut reader, mut client) = client(lib.clone()).await;
    for (id, name, args) in [
        (
            2,
            "full_text_search",
            json!({"query":"x", "mode":"invalid"}),
        ),
        (3, "full_text_search", json!({"query":"x", "offset":10001})),
        (
            4,
            "search_facet_counts",
            json!({"query":"x", "mode":"invalid"}),
        ),
    ] {
        let response = request(
            &mut reader,
            &mut client,
            id,
            "tools/call",
            json!({"name":name,"arguments":args}),
        )
        .await;
        assert_eq!(response["error"]["code"], -32602, "{response}");
    }
    let response = request(
        &mut reader,
        &mut client,
        5,
        "tools/call",
        json!({"name":"full_text_search","arguments":{"query":"x"}}),
    )
    .await;
    assert_eq!(response["error"]["data"]["reason"], "queue_full");
    for waiter in waiters {
        waiter.abort();
    }
    drop(held);
}

#[tokio::test]
async fn watcher_refreshes_after_entering_a_full_search_queue() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("test.db");
    let writer = rusqlite::Connection::open(&path).unwrap();
    writer.execute_batch("CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT); INSERT INTO schema_meta VALUES ('db_version','32'); CREATE TABLE book(id INTEGER PRIMARY KEY, title TEXT, totalLines INTEGER);").unwrap();
    let lib = library(path);
    let original = lib.current().fingerprint.clone();
    let held = lib
        .search_admission()
        .acquire(CancellationToken::new())
        .await
        .unwrap();
    let mut waiters = Vec::new();
    for _ in 0..32 {
        let lib = lib.clone();
        waiters.push(tokio::spawn(async move {
            lib.search_admission()
                .acquire(CancellationToken::new())
                .await
                .unwrap()
                .run(|| ())
                .await
                .unwrap();
        }));
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while lib.search_admission().metrics().in_flight != 33 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    writer
        .execute(
            "UPDATE schema_meta SET value='33' WHERE key='db_version'",
            [],
        )
        .unwrap();
    let watcher = tokio::spawn(lib.clone().watch());
    tokio::time::timeout(Duration::from_secs(12), async {
        while lib.search_admission().metrics().in_flight != 34 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("watcher failed to enter the reserved slot");
    assert_eq!(lib.current().fingerprint, original);
    drop(held);
    for waiter in waiters {
        waiter.await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while lib.current().fingerprint == original {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("watcher did not publish refreshed snapshot");
    assert_eq!(lib.current().db.schema_version(), Some(33));
    lib.shutdown();
    watcher.abort();
}
