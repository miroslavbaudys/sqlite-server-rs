//! Reference Rust client for sqlite-server-rs.
//!
//! A counterpart to `examples/python/sqlite.py`: it handles the length-prefixed JSON
//! framing, client-side `?` / `?N` parameter binding with escaping, and a typed result
//! wrapper (`QueryResult` / `Row`). Copy this file into your project as a starting point,
//! or run the demo at the bottom against a live server:
//!
//! ```sh
//! # in one terminal
//! mkdir -p /tmp/sqlite-data
//! cargo run -- --databases-folder /tmp/sqlite-data
//!
//! # in another
//! cargo run --example client
//! ```
//!
//! It depends only on `serde_json` and `regex`, which the server crate already pulls in.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};

use regex::{Captures, Regex};
use serde_json::{json, Map, Value};

/// Default endpoint, matching the server's defaults.
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 3333;

// ---------------------------------------------------------------------------
// Parameter binding
// ---------------------------------------------------------------------------

/// A value bound to a `?` / `?N` placeholder. Each is escaped client-side into a SQL
/// literal before the query is sent (the server has no separate bind step).
#[derive(Debug, Clone)]
pub enum Param {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl From<i64> for Param {
    fn from(v: i64) -> Self {
        Param::Int(v)
    }
}
impl From<i32> for Param {
    fn from(v: i32) -> Self {
        Param::Int(v as i64)
    }
}
impl From<f64> for Param {
    fn from(v: f64) -> Self {
        Param::Real(v)
    }
}
impl From<bool> for Param {
    fn from(v: bool) -> Self {
        Param::Bool(v)
    }
}
impl From<&str> for Param {
    fn from(v: &str) -> Self {
        Param::Text(v.to_string())
    }
}
impl From<String> for Param {
    fn from(v: String) -> Self {
        Param::Text(v)
    }
}
impl From<Vec<u8>> for Param {
    fn from(v: Vec<u8>) -> Self {
        Param::Blob(v)
    }
}
impl<T: Into<Param>> From<Option<T>> for Param {
    fn from(v: Option<T>) -> Self {
        match v {
            Some(x) => x.into(),
            None => Param::Null,
        }
    }
}

/// Sanitize a value into a SQL literal, mirroring the Python client's `_serialize_sql_value`.
fn serialize_sql_value(param: &Param) -> String {
    match param {
        Param::Null => "NULL".to_string(),
        // SQLite stores booleans as 0/1.
        Param::Bool(b) => if *b { "1" } else { "0" }.to_string(),
        Param::Int(n) => n.to_string(),
        Param::Real(f) => {
            // SQLite has no NaN/Infinity literals: NaN -> NULL, +/-Inf -> +/-9e999.
            if f.is_nan() {
                "NULL".to_string()
            } else if f.is_infinite() {
                if *f > 0.0 { "9e999" } else { "-9e999" }.to_string()
            } else {
                f.to_string()
            }
        }
        // Escape single quotes by doubling them.
        Param::Text(s) => format!("'{}'", s.replace('\'', "''")),
        // Emit a BLOB literal X'..' so binary data round-trips without corruption.
        Param::Blob(bytes) => format!("X'{}'", to_hex(bytes)),
    }
}

/// Replace `?` and `?N` placeholders in a single left-to-right pass, so a substituted
/// value (which may itself contain `?`) is never re-scanned.
fn bind(sql: &str, params: &[Param]) -> String {
    // `?N` is tried before bare `?` via alternation, exactly like the Python client.
    let re = Regex::new(r"\?(\d+)|\?").expect("valid regex");
    let mut next = 0usize;

    re.replace_all(sql, |caps: &Captures| {
        if let Some(digits) = caps.get(1) {
            // Positional ?N -> params[N - 1].
            let index: usize = digits.as_str().parse().unwrap_or(0);
            if index >= 1 && index <= params.len() {
                serialize_sql_value(&params[index - 1])
            } else {
                caps[0].to_string()
            }
        } else if next < params.len() {
            // Standard ? -> next not-yet-consumed value.
            let value = serialize_sql_value(&params[next]);
            next += 1;
            value
        } else {
            "?".to_string()
        }
    })
    .into_owned()
}

// ---------------------------------------------------------------------------
// Result wrappers
// ---------------------------------------------------------------------------

/// A single result row: column name -> JSON value (keys arrive alphabetically sorted).
#[derive(Debug, Clone)]
pub struct Row(Map<String, Value>);

impl Row {
    pub fn get(&self, column: &str) -> Option<&Value> {
        self.0.get(column)
    }
    pub fn get_i64(&self, column: &str) -> Option<i64> {
        self.0.get(column)?.as_i64()
    }
    pub fn get_f64(&self, column: &str) -> Option<f64> {
        self.0.get(column)?.as_f64()
    }
    pub fn get_str(&self, column: &str) -> Option<&str> {
        self.0.get(column)?.as_str()
    }
    pub fn is_null(&self, column: &str) -> bool {
        matches!(self.0.get(column), Some(Value::Null) | None)
    }
    /// Decode a BLOB column from its `X'..'` hex literal into bytes. `None` if the column
    /// is NULL/absent or is not a BLOB literal.
    pub fn blob(&self, column: &str) -> Option<Vec<u8>> {
        decode_blob_literal(self.0.get(column)?.as_str()?)
    }
}

/// A read-only view over a query response.
#[derive(Debug, Clone)]
pub struct QueryResult {
    payload: Value,
}

impl QueryResult {
    fn new(payload: Value) -> Self {
        Self { payload }
    }

    /// The column names in `SELECT` order (authoritative; a row's own keys are alphabetical).
    pub fn columns(&self) -> Vec<String> {
        self.payload
            .get("columns")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The rows as typed `Row`s.
    pub fn rows(&self) -> Vec<Row> {
        self.data()
            .iter()
            .filter_map(|v| v.as_object().cloned().map(Row))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.data().len()
    }
    pub fn is_empty(&self) -> bool {
        self.data().is_empty()
    }

    pub fn first(&self) -> Option<Row> {
        self.rows().into_iter().next()
    }

    /// The first column of the first row — ideal for `COUNT(*)` / `MAX(...)` queries.
    pub fn scalar(&self) -> Option<Value> {
        let first = self.data().first()?.as_object()?;
        let key = self
            .columns()
            .into_iter()
            .next()
            .or_else(|| first.keys().next().cloned())?;
        first.get(&key).cloned()
    }
    pub fn scalar_i64(&self) -> Option<i64> {
        self.scalar()?.as_i64()
    }

    /// `Some(message)` if the server reported an error instead of a result set.
    pub fn server_error(&self) -> Option<String> {
        let obj = self.payload.as_object()?;
        if let Some(msg) = obj.get("error_message").and_then(Value::as_str) {
            return Some(msg.to_string());
        }
        if obj.contains_key("error_code") || obj.contains_key("generic_error") {
            return Some(self.payload.to_string());
        }
        None
    }

    fn data(&self) -> &[Value] {
        self.payload
            .get("data")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// A client connection to sqlite-server-rs, bound to a single database name.
pub struct Sqlite {
    stream: TcpStream,
    database: String,
}

impl Sqlite {
    /// Connect to the default endpoint (127.0.0.1:3333).
    pub fn connect(database: &str) -> io::Result<Self> {
        Self::connect_to(database, (DEFAULT_HOST, DEFAULT_PORT))
    }

    /// Connect to a specific endpoint.
    pub fn connect_to<A: ToSocketAddrs>(database: &str, addr: A) -> io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        Ok(Self {
            stream,
            database: database.to_string(),
        })
    }

    /// Run a SQL statement against this connection's database, binding `params` into
    /// `?` / `?N` placeholders client-side.
    pub fn query(&mut self, sql: &str, params: &[Param]) -> io::Result<QueryResult> {
        let prepared = if params.is_empty() {
            sql.to_string()
        } else {
            bind(sql, params)
        };
        let response = self.call(&json!({
            "cmd": "QUERY",
            "db": self.database,
            "query": prepared,
        }))?;
        Ok(QueryResult::new(response))
    }

    /// List the database files available on the server.
    pub fn list(&mut self) -> io::Result<Vec<String>> {
        let response = self.call(&json!({ "cmd": "LIST" }))?;
        Ok(response
            .get("list")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Delete a database file. Returns `true` if it was removed.
    pub fn delete_db(&mut self, database: &str) -> io::Result<bool> {
        let response = self.call(&json!({ "cmd": "DELETE_DB", "db": database }))?;
        Ok(response.get("result").and_then(Value::as_str) == Some("ok"))
    }

    /// Send one request and read one response (raw JSON), handling the framing.
    pub fn call(&mut self, request: &Value) -> io::Result<Value> {
        self.send(request)?;
        self.recv()
    }

    fn send(&mut self, request: &Value) -> io::Result<()> {
        let body = serde_json::to_vec(request)?;
        self.stream.write_all(&(body.len() as u32).to_le_bytes())?;
        self.stream.write_all(&body)?;
        self.stream.flush()
    }

    fn recv(&mut self) -> io::Result<Value> {
        let mut header = [0u8; 4];
        self.stream.read_exact(&mut header)?;
        let size = u32::from_le_bytes(header) as usize;
        let mut buf = vec![0u8; size];
        self.stream.read_exact(&mut buf)?;
        serde_json::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// Decode an `X'<hex>'` BLOB literal into bytes. Returns `None` for non-literals.
fn decode_blob_literal(literal: &str) -> Option<Vec<u8>> {
    let bytes = literal.as_bytes();
    if bytes.len() < 3 || !(bytes[0] == b'X' || bytes[0] == b'x') || bytes[1] != b'\'' {
        return None;
    }
    if *bytes.last()? != b'\'' {
        return None;
    }
    let hex = &literal[2..literal.len() - 1];
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let h = hex.as_bytes();
    let mut out = Vec::with_capacity(hex.len() / 2);
    let mut i = 0;
    while i < h.len() {
        out.push((hex_val(h[i])? << 4) | hex_val(h[i + 1])?);
        i += 2;
    }
    Some(out)
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Demo
// ---------------------------------------------------------------------------

fn main() -> io::Result<()> {
    let mut db = match Sqlite::connect("example.db") {
        Ok(db) => db,
        Err(e) => {
            eprintln!(
                "Could not connect to {DEFAULT_HOST}:{DEFAULT_PORT} — is the server running? ({e})"
            );
            std::process::exit(1);
        }
    };

    db.query(
        "CREATE TABLE IF NOT EXISTS users(id INTEGER, name TEXT, score REAL, data BLOB)",
        &[],
    )?;
    db.query("DELETE FROM users", &[])?;

    // `?` placeholders are escaped client-side; bytes become X'..' BLOB literals.
    db.query(
        "INSERT INTO users VALUES (?, ?, ?, ?)",
        &[1.into(), "Alice".into(), 9.5.into(), vec![0u8, 255].into()],
    )?;
    db.query(
        "INSERT INTO users VALUES (?, ?, ?, ?)",
        &[2.into(), "Bob O'Brien".into(), 3.0.into(), Param::Null],
    )?;

    let result = db.query("SELECT id, name, score, data FROM users ORDER BY id", &[])?;
    if let Some(err) = result.server_error() {
        eprintln!("server error: {err}");
        std::process::exit(1);
    }

    println!("columns: {:?}", result.columns());
    for row in result.rows() {
        println!(
            "  id={:?} name={:?} score={:?} data={:?}",
            row.get_i64("id"),
            row.get_str("name"),
            row.get_f64("score"),
            row.blob("data"),
        );
    }

    let count = db
        .query("SELECT COUNT(*) AS n FROM users", &[])?
        .scalar_i64()
        .unwrap_or(0);
    println!("count: {count}");
    println!("databases on server: {:?}", db.list()?);

    Ok(())
}
