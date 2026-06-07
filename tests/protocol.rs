//! End-to-end protocol tests: boot the actual binary against a temp folder and speak the
//! length-prefixed JSON protocol over a real TCP socket.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

use serde_json::{json, Value};

/// A running server bound to an ephemeral-ish port, cleaned up on drop.
struct TestServer {
    child: Child,
    port: u16,
    _dir: PathBuf,
}

impl TestServer {
    fn start() -> TestServer {
        // Unique temp folder + port per test run.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let port = 20000 + (nanos % 20000) as u16;
        let dir = std::env::temp_dir().join(format!("sqlite-server-rs-test-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();

        let bin = env!("CARGO_BIN_EXE_sqlite-server");
        let child = Command::new(bin)
            .args([
                "--ip",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--databases-folder",
                dir.to_str().unwrap(),
            ])
            .spawn()
            .expect("spawn server");

        // Wait for the listener to come up.
        for _ in 0..100 {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        TestServer {
            child,
            port,
            _dir: dir,
        }
    }

    fn connect(&self) -> TcpStream {
        let stream = TcpStream::connect(("127.0.0.1", self.port)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self._dir);
    }
}

/// Send one framed request and read one framed response on `stream`.
fn call(stream: &mut TcpStream, request: &Value) -> Value {
    let body = serde_json::to_vec(request).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    stream.write_all(&out).unwrap();
    read_response(stream)
}

/// Like `call`, but sends a raw (possibly non-JSON) body to test the parser/repair path.
fn call_raw(stream: &mut TcpStream, body: &str) -> Value {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
    stream.write_all(&out).unwrap();
    read_response(stream)
}

fn read_response(stream: &mut TcpStream) -> Value {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).unwrap();
    let len = u32::from_le_bytes(header) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).unwrap();
    serde_json::from_slice(&buf).unwrap()
}

#[test]
fn full_protocol_flow() {
    let server = TestServer::start();
    let mut conn = server.connect();

    // CREATE returns empty columns/data.
    let resp = call(
        &mut conn,
        &json!({"cmd": "QUERY", "db": "test.db",
                "query": "CREATE TABLE users(id INTEGER, name TEXT, score REAL, data BLOB)"}),
    );
    assert_eq!(resp, json!({"columns": [], "data": []}));

    // INSERT rows including a NULL, a float, and a blob literal.
    call(
        &mut conn,
        &json!({"cmd": "QUERY", "db": "test.db",
                "query": "INSERT INTO users VALUES (1, 'Alice', 9.5, X'00ff'), (2, NULL, 3.0, NULL)"}),
    );

    // SELECT: columns preserve SELECT order; row objects carry typed values.
    let resp = call(
        &mut conn,
        &json!({"cmd": "QUERY", "db": "test.db",
                "query": "SELECT id, name, score, data FROM users ORDER BY id"}),
    );
    assert_eq!(resp["columns"], json!(["id", "name", "score", "data"]));
    let rows = resp["data"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"], json!(1));
    assert_eq!(rows[0]["name"], json!("Alice"));
    assert_eq!(rows[0]["score"], json!(9.5));
    assert_eq!(rows[0]["data"], json!("X'00ff'")); // BLOB -> lowercase hex literal
    assert_eq!(rows[1]["name"], Value::Null); // NULL -> json null

    // LIST shows the database file (and not sidecars).
    let resp = call(&mut conn, &json!({"cmd": "LIST"}));
    let list: Vec<String> = resp["list"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(list.contains(&"test.db".to_string()));
    assert!(!list.iter().any(|n| n.ends_with("-wal")));

    // DELETE_DB removes it.
    let resp = call(&mut conn, &json!({"cmd": "DELETE_DB", "db": "test.db"}));
    assert_eq!(resp, json!({"result": "ok"}));
    let resp = call(&mut conn, &json!({"cmd": "DELETE_DB", "db": "test.db"}));
    assert_eq!(resp, json!({"result": "error"})); // already gone
}

#[test]
fn error_responses() {
    let server = TestServer::start();
    let mut conn = server.connect();

    // Invalid JSON that the key-repair regex cannot save.
    let resp = call_raw(&mut conn, "not json at all {{{");
    assert_eq!(resp["generic_error"], json!(0)); // INVALID_FORMAT
    assert!(resp.get("message").is_some());

    // Missing cmd.
    let resp = call(&mut conn, &json!({"db": "x"}));
    assert_eq!(resp["generic_error"], json!(1)); // NO_COMMAND_SPECIFIED

    // Unknown command.
    let resp = call(&mut conn, &json!({"cmd": "FROBNICATE"}));
    assert_eq!(resp["generic_error"], json!(2)); // UNKNOWN_COMMAND

    // QUERY without db / without query.
    let resp = call(&mut conn, &json!({"cmd": "QUERY", "query": "SELECT 1"}));
    assert_eq!(resp["generic_error"], json!(3)); // NO_DATABASE_SPECIFIED
    let resp = call(&mut conn, &json!({"cmd": "QUERY", "db": "ok.db"}));
    assert_eq!(resp["generic_error"], json!(4)); // ERROR_READING_FROM_CLIENT

    // Path traversal is rejected.
    let resp = call(
        &mut conn,
        &json!({"cmd": "QUERY", "db": "../escape.db", "query": "SELECT 1"}),
    );
    assert_eq!(resp["generic_error"], json!(3)); // NO_DATABASE_SPECIFIED

    // SQL error carries the SQLite code + message and echoes the request.
    let resp = call(
        &mut conn,
        &json!({"cmd": "QUERY", "db": "ok.db", "query": "SELECT * FROM ghosts"}),
    );
    assert_eq!(resp["error_code"], json!(1)); // SQLITE_ERROR
    assert!(resp["error_message"]
        .as_str()
        .unwrap()
        .contains("no such table"));

    // Empty query -> SQLITE_MISUSE (21) "empty query".
    let resp = call(
        &mut conn,
        &json!({"cmd": "QUERY", "db": "ok.db", "query": "   "}),
    );
    assert_eq!(resp["error_code"], json!(21));
    assert_eq!(resp["error_message"], json!("empty query"));
}

#[test]
fn case_insensitive_and_key_repair() {
    let server = TestServer::start();
    let mut conn = server.connect();

    // Command matched case-insensitively.
    let resp = call(&mut conn, &json!({"cmd": "list"}));
    assert!(resp.get("list").is_some());

    // Unquoted keys repaired before parsing (SQLiteStudio style).
    let resp = call_raw(&mut conn, r#"{cmd:"LIST"}"#);
    assert!(resp.get("list").is_some());
}
