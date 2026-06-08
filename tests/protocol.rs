//! End-to-end protocol tests: boot the actual binary against a temp folder and speak the
//! length-prefixed JSON protocol over a real TCP socket.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{json, Value};

/// A running server bound to an OS-assigned ephemeral port, cleaned up on drop.
struct TestServer {
    child: Child,
    port: u16,
    _dir: PathBuf,
}

impl TestServer {
    fn start() -> TestServer {
        Self::start_with_args(&[])
    }

    /// Start a server with additional CLI flags (e.g. `--auth`, `--ip-whitelist`).
    fn start_with_args(extra: &[&str]) -> TestServer {
        // A process-unique temp folder (counter avoids collisions between parallel tests).
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("sqlite-server-rs-test-{nanos}-{id}"));
        std::fs::create_dir_all(&dir).unwrap();

        let bin = env!("CARGO_BIN_EXE_sqlite3-server");
        // Port 0 -> the OS picks a free port, eliminating port-collision races between
        // parallel tests. We learn the real port from the server's startup log line.
        let mut child = Command::new(bin)
            .args([
                "--ip",
                "127.0.0.1",
                "--port",
                "0",
                "--databases-folder",
                dir.to_str().unwrap(),
            ])
            .args(extra)
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn server");

        let port = read_listen_port(&mut child);

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

/// Block until the server prints its startup line, then parse the bound port from it.
/// The line looks like: `sqlite-server listening on 127.0.0.1:54321 (...)`.
fn read_listen_port(child: &mut Child) -> u16 {
    let stdout = child.stdout.take().expect("piped stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).expect("read server stdout");
        if n == 0 {
            panic!("server exited before reporting a listen address");
        }
        if let Some(rest) = line.split("listening on ").nth(1) {
            let addr = rest.split_whitespace().next().expect("address token");
            let port = addr
                .rsplit(':')
                .next()
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or_else(|| panic!("could not parse port from line: {line:?}"));

            // Drain the rest of stdout in the background so a full pipe never blocks the server.
            std::thread::spawn(move || {
                let mut sink = Vec::new();
                let _ = reader.read_to_end(&mut sink);
            });
            return port;
        }
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

#[test]
fn oversized_packet_closes_connection() {
    let server = TestServer::start();
    let mut conn = server.connect();

    // Claim a length larger than the default 16 MiB max; the server must close without
    // reading a body or sending a response.
    let oversized: u32 = 20 * 1024 * 1024;
    conn.write_all(&oversized.to_le_bytes()).unwrap();

    // Reading a response header should now fail (connection closed).
    let mut header = [0u8; 4];
    assert!(conn.read_exact(&mut header).is_err());
}

#[test]
fn transaction_rollback_on_one_connection() {
    let server = TestServer::start();
    let mut conn = server.connect();

    call(
        &mut conn,
        &json!({"cmd":"QUERY","db":"tx.db","query":"CREATE TABLE t(id INTEGER)"}),
    );
    // Issued as separate requests on the same persistent connection.
    call(
        &mut conn,
        &json!({"cmd":"QUERY","db":"tx.db","query":"BEGIN"}),
    );
    call(
        &mut conn,
        &json!({"cmd":"QUERY","db":"tx.db","query":"INSERT INTO t VALUES (1), (2)"}),
    );
    call(
        &mut conn,
        &json!({"cmd":"QUERY","db":"tx.db","query":"ROLLBACK"}),
    );

    let resp = call(
        &mut conn,
        &json!({"cmd":"QUERY","db":"tx.db","query":"SELECT COUNT(*) AS n FROM t"}),
    );
    assert_eq!(resp["data"][0]["n"], json!(0));
}

#[test]
fn auth_required_handshake() {
    let server = TestServer::start_with_args(&["--auth", "s3cret"]);
    let mut conn = server.connect();

    // A command before authenticating is rejected.
    assert_eq!(
        call(&mut conn, &json!({"cmd": "LIST"})),
        json!({"result": "error"})
    );
    // Wrong password is rejected.
    assert_eq!(
        call(&mut conn, &json!({"auth": "nope"})),
        json!({"result": "error"})
    );
    // Correct password authenticates this connection.
    assert_eq!(
        call(&mut conn, &json!({"auth": "s3cret"})),
        json!({"result": "ok"})
    );
    // Now commands work.
    assert!(call(&mut conn, &json!({"cmd": "LIST"}))
        .get("list")
        .is_some());

    // Authentication is per-connection: a fresh socket must authenticate again.
    let mut conn2 = server.connect();
    assert_eq!(
        call(&mut conn2, &json!({"cmd": "LIST"})),
        json!({"result": "error"})
    );
}

#[test]
fn ip_whitelist_allows_listed_peer() {
    let server = TestServer::start_with_args(&["--ip-whitelist", "127.0.0.1/32"]);
    let mut conn = server.connect();
    assert!(call(&mut conn, &json!({"cmd": "LIST"}))
        .get("list")
        .is_some());
}

#[test]
fn ip_whitelist_rejects_other_peers() {
    // Whitelist a network that excludes localhost; the connection must be dropped at accept.
    let server = TestServer::start_with_args(&["--ip-whitelist", "10.0.0.0/8"]);
    let mut conn = server.connect();

    let body = serde_json::to_vec(&json!({"cmd": "LIST"})).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    // The write may buffer locally, but no response will ever come back.
    let _ = conn.write_all(&out);

    let mut header = [0u8; 4];
    assert!(conn.read_exact(&mut header).is_err());
}

#[test]
fn two_connections_share_a_database_file() {
    let server = TestServer::start();
    let mut writer = server.connect();
    let mut reader = server.connect();

    call(
        &mut writer,
        &json!({"cmd":"QUERY","db":"shared.db","query":"CREATE TABLE s(v TEXT)"}),
    );
    // Autocommitted by the writer connection.
    call(
        &mut writer,
        &json!({"cmd":"QUERY","db":"shared.db","query":"INSERT INTO s VALUES ('hello')"}),
    );

    // A separate connection (separate SQLite handle to the same file) sees it.
    let resp = call(
        &mut reader,
        &json!({"cmd":"QUERY","db":"shared.db","query":"SELECT v FROM s"}),
    );
    assert_eq!(resp["data"][0]["v"], json!("hello"));
}
