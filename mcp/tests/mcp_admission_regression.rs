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

fn reference_fixture(path: &std::path::Path) -> rusqlite::Connection {
    let db = rusqlite::Connection::open(path).unwrap();
    db.execute_batch(
        "CREATE TABLE book (id INTEGER PRIMARY KEY, title TEXT, heRef TEXT,
                               isBaseBook INTEGER, orderIndex INTEGER, totalLines INTEGER);
             CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER,
                               heRef TEXT, content TEXT);
             CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO schema_meta VALUES ('db_version', '39');
             INSERT INTO book VALUES (1, 'אוצר חכמה', 'אוצר חכמה', 1, 0, 3);
             INSERT INTO line VALUES
                (1, 1, 0, 'אוצר חכמה', 'כותרת'),
                (2, 1, 1, 'אוצר חכמה, חכמה', 'תוכן הסעיף'),
                (3, 1, 2, 'אוצר חכמה, תיבה נדירה', 'תוכן נוסף');",
    )
    .unwrap();
    db
}

#[tokio::test]
async fn resolve_refs_reuses_results_and_preserves_single_lookup_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("test.db");
    drop(reference_fixture(&path));
    let lib = library(path);
    let (mut reader, mut writer) = client(lib.clone()).await;
    let response = request(
        &mut reader,
        &mut writer,
        2,
        "tools/call",
        json!({
            "name": "resolve_refs",
            "arguments": {
                "references": [
                    "אוצר חכמה חכמה", "אוצר חכמה", " אוצר חכמה חכמה ", "תיבה נדירה", " "
                ],
                "exclude_book_level": true,
                "snippets": false
            }
        }),
    )
    .await;
    let batch = payload(&response);
    let results = batch["results"].as_array().unwrap();
    assert_eq!(batch["count"], 5);
    assert_eq!(batch["schema_version"], 39);
    assert_eq!(results[0]["candidates"][0]["line_index"], 1);
    assert_eq!(results[0]["candidates"][0]["snippet"], "");
    assert!(results[0]["candidates"][0].get("book_level").is_none());
    assert_eq!(results[1]["count"], 0);
    assert_eq!(results[0], results[2]);
    assert_eq!(results[3]["count"], 0, "batch fallback is opt-in");
    assert!(results[4]["error"].is_string());
    assert_eq!(lib.read_admission().metrics().completed, 3);

    let response = request(
        &mut reader,
        &mut writer,
        3,
        "tools/call",
        json!({"name":"resolve_ref", "arguments":{"reference":"תיבה נדירה"}}),
    )
    .await;
    let single = payload(&response);
    assert_eq!(single["candidates"][0]["match_kind"], "fallback");
    assert_eq!(single["candidates"][0]["snippet"], "תוכן נוסף");
    assert_eq!(single["resolver"], batch["resolver"]);
    assert_eq!(single["data_version"], batch["data_version"]);

    let response = request(
        &mut reader,
        &mut writer,
        4,
        "tools/call",
        json!({
            "name":"resolve_refs",
            "arguments":{"references":["תיבה נדירה", "אוצר חכמה"], "heref_fallback":true}
        }),
    )
    .await;
    let batch = payload(&response);
    assert_eq!(
        batch["results"][0]["candidates"][0]["match_kind"],
        "fallback"
    );
    assert_eq!(batch["results"][1]["candidates"][0]["book_level"], true);

    let response = request(
        &mut reader,
        &mut writer,
        5,
        "tools/call",
        json!({
            "name":"resolve_refs",
            "arguments":{"references":vec!["אוצר חכמה"; 401], "limit":50}
        }),
    )
    .await;
    assert_eq!(response["error"]["code"], -32602, "{response}");
}

async fn wait_for_reads(gate: &otzaria_mcp::admission::Gate, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while gate.metrics().in_flight != count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn hold_between_batch_references(
    gate: &otzaria_mcp::admission::Gate,
    writer: &mut Writer,
) -> otzaria_mcp::admission::Admitted {
    let held = gate.acquire(CancellationToken::new()).await.unwrap();
    let batch = json!({
        "jsonrpc":"2.0", "id":2, "method":"tools/call",
        "params":{"name":"resolve_refs","arguments":{"references":["אוצר חכמה", "אוצר חכמה חכמה"]}}
    });
    writer
        .write_all(format!("{batch}\n").as_bytes())
        .await
        .unwrap();
    wait_for_reads(gate, 2).await;

    let other_reader = tokio::spawn({
        let gate = gate.clone();
        async move { gate.acquire(CancellationToken::new()).await.unwrap() }
    });
    wait_for_reads(gate, 3).await;
    drop(held);
    let other_reader = tokio::time::timeout(Duration::from_secs(5), other_reader)
        .await
        .unwrap()
        .unwrap();
    wait_for_reads(gate, 2).await;
    assert_eq!(gate.metrics().completed, 1, "only the first reference ran");
    other_reader
}

#[tokio::test]
async fn resolve_refs_yields_to_another_reader_and_stops_on_cancellation() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("test.db");
    drop(reference_fixture(&path));
    let lib = library(path);
    let (mut reader, mut writer) = client(lib.clone()).await;
    let gate = lib.read_admission();
    let other_reader = hold_between_batch_references(gate, &mut writer).await;
    writer
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":2}}\n")
        .await
        .unwrap();
    wait_for_reads(gate, 1).await;
    drop(other_reader);
    let single = json!({
        "jsonrpc":"2.0", "id":3, "method":"tools/call",
        "params":{"name":"resolve_ref","arguments":{"reference":"אוצר חכמה"}}
    });
    writer
        .write_all(format!("{single}\n").as_bytes())
        .await
        .unwrap();
    loop {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(n, 0);
        let response: Value = serde_json::from_str(&line).unwrap();
        if response["id"] == 3 {
            assert_eq!(payload(&response)["count"], 1);
            break;
        }
        assert_eq!(response["id"], 2);
        assert!(response.get("error").is_some(), "{response}");
    }
    assert_eq!(gate.metrics().completed, 2);
}

#[tokio::test]
async fn resolve_refs_rejects_a_wal_commit_between_references() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("test.db");
    let db = reference_fixture(&path);
    db.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    let lib = library(path);
    let (mut reader, mut writer) = client(lib.clone()).await;
    let other_reader = hold_between_batch_references(lib.read_admission(), &mut writer).await;
    db.execute("UPDATE line SET content='תוכן מעודכן' WHERE id=2", [])
        .unwrap();
    drop(other_reader);

    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["id"], 2);
    assert_eq!(response["error"]["data"]["reason"], "library_changed");

    let response = request(
        &mut reader,
        &mut writer,
        3,
        "tools/call",
        json!({"name":"resolve_refs", "arguments":{"references":["אוצר חכמה חכמה"]}}),
    )
    .await;
    assert_eq!(
        payload(&response)["results"][0]["candidates"][0]["snippet"],
        "תוכן מעודכן",
    );
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
    assert_ne!(
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
