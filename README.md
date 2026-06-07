# sqlite-server-rs

A Rust port of [sqlite-server](https://github.com/) — a small, multi-threaded TCP server
that exposes [SQLite](https://sqlite.org) databases over a length-prefixed JSON protocol.
Clients open a socket, send a JSON request, and receive a JSON response. The protocol is
**wire-compatible** with the original C++ server, so existing clients (including the
reference Python client) work unchanged.

- **Transport:** raw TCP, one 4-byte little-endian length header + UTF-8 JSON body per message.
- **Commands:** `QUERY`, `LIST`, `DELETE_DB`.
- **Concurrency:** a Tokio multi-threaded runtime with a configurable worker count.
- **Storage:** one SQLite database file per name, kept in a configured folder.

---

## Building

The build needs only the Rust toolchain (`cargo`/`rustc`). SQLite is compiled from source
via `rusqlite`'s `bundled` feature, so there is no system SQLite dependency.

```sh
cargo build --release      # -> target/release/sqlite-server
```

| Dependency | Purpose |
|------------|---------|
| `tokio` | async runtime + worker-thread pool (analogue of Boost.Asio) |
| `serde_json` | JSON parsing / serialization |
| `rusqlite` (`bundled`) | SQLite engine, compiled from source |
| `clap` | CLI parsing |
| `regex` | lenient JSON key-repair fallback |

---

## Running

```sh
./target/release/sqlite-server --port 3333 --databases-folder ./data
```

### Command-line options

| Option | Default | Description |
|--------|---------|-------------|
| `-h, --help` | — | Show usage and exit |
| `-v, --version` | — | Show git branch + commit and exit |
| `-c, --config <path>` | — | Load settings from a JSON config file (see below) |
| `--ip <ip>` | `localhost` | Listen address |
| `-p, --port <port>` | `3333` | Listen port |
| `-d, --databases-folder <dir>` | `sqlite` | Folder holding the database files (must exist) |
| `-w, --workers <n>` | CPU cores | Number of worker threads |
| `--client-max-packet-size <bytes>` | `16777216` (16 MiB) | Max request size; larger requests close the connection |

### Config file

When `--config` is given, all settings come from the JSON file and the other flags are ignored:

```json
{
  "client_max_packet_size": 16777216,
  "workers": 4,
  "listen_ip": "127.0.0.1",
  "listen_port": 3333,
  "databases_folder": "./data"
}
```

The databases folder **must already exist** — the server will not create it (individual
database files inside it are created on demand). Shutdown is graceful on `SIGINT`/`SIGTERM`.

---

## Communication protocol

### Framing

Every message — in both directions — is a 4-byte little-endian `uint32` length `N`,
followed by `N` bytes of UTF-8 JSON. Read the 4 bytes, decode the length, then read
exactly that many bytes.

### Connection lifecycle

A connection is **persistent**: requests on a single connection are processed
sequentially (send a request, read its full response, then send the next).

### Request format

A request is a JSON object with a `cmd` field (matched case-insensitively) plus
command-specific fields.

| Command | Required fields | Purpose |
|---------|-----------------|---------|
| `QUERY` | `db`, `query` | Run a single SQL statement against database `db` |
| `LIST` | — | List available database files |
| `DELETE_DB` | `db` | Delete the database file `db` |

> Tolerance: lightly-malformed JSON with unquoted keys (e.g. `{cmd:"LIST"}`) is repaired
> before parsing.

### `QUERY`

```json
{ "cmd": "QUERY", "db": "mydb", "query": "SELECT id, name FROM users WHERE id = 1" }
```

Success — `columns` is the column list in `SELECT` order, `data` is an array of row objects:

```json
{ "columns": ["id", "name"], "data": [ { "id": 1, "name": "Alice" } ] }
```

Statements with no result set (`INSERT`, `CREATE`, …) return `{ "columns": [], "data": [] }`.

#### Value encoding

| SQLite type | JSON representation |
|-------------|---------------------|
| INTEGER | number (64-bit) |
| FLOAT | number |
| TEXT | string |
| NULL | `null` |
| BLOB | string `"X'<hex>'"` (lowercase), e.g. `"X'00ff'"` |

Each row is a JSON object whose keys are serialized **alphabetically** — use the top-level
`columns` array when you need the original `SELECT` order.

### `LIST`

```json
{ "cmd": "LIST" }
```

Returns the regular files in the databases folder, excluding SQLite sidecar files
(`-wal`, `-shm`, `-journal`): `{ "list": ["mydb", "sales", "logs"] }`.

### `DELETE_DB`

```json
{ "cmd": "DELETE_DB", "db": "mydb" }
```

Response: `{ "result": "ok" }` if the file was removed, `{ "result": "error" }` otherwise.

### Database name rules

`db` must be a **plain file name inside the configured folder**. Path separators, absolute
paths, `.`/`..` traversal, and symlinks pointing outside the folder are rejected. A
non-existent database is created on first `QUERY`.

### Error responses

Command/validation errors carry a numeric `generic_error` code and echo the `request`:

| Code | Name | Meaning |
|------|------|---------|
| 0 | `INVALID_FORMAT` | Request body was not valid JSON (also includes a `message`) |
| 1 | `NO_COMMAND_SPECIFIED` | Missing `cmd` |
| 2 | `UNKNOWN_COMMAND` | `cmd` is not a supported command |
| 3 | `NO_DATABASE_SPECIFIED` | Missing `db`, or `db` is not a safe/valid name |
| 4 | `ERROR_READING_FROM_CLIENT` | Missing `query` on a `QUERY` request |

SQL errors carry the SQLite primary result code, its message, and the original request:

```json
{ "error_code": 1, "error_message": "no such table: ghosts",
  "query": { "cmd": "QUERY", "db": "mydb", "query": "SELECT * FROM ghosts" } }
```

An empty/whitespace query is reported as `error_code` 21 (`SQLITE_MISUSE`), `"empty query"`.

---

## Python client

A ready-to-use, dependency-free client lives at
[`examples/python/sqlite.py`](examples/python/sqlite.py) (standard library only). It
handles framing, client-side `?` parameter binding with escaping, and a typed result
wrapper. Copy it into your project and use it directly:

```python
from sqlite import Sqlite

# Connects to 127.0.0.1:3333 by default (edit _SQLITE_IP / _SQLITE_PORT to change).
with Sqlite("mydb") as db:
    db.send_query("CREATE TABLE IF NOT EXISTS users(id INTEGER, name TEXT)")
    db.send_query("INSERT INTO users VALUES(?, ?)", [1, "Alice"])   # ? params are escaped

    result = db.query("SELECT id, name FROM users WHERE id = ?", [1])
    for row in result:
        print(row.id, row.name)          # rows support both row["id"] and row.id

    n = db.query("SELECT COUNT(*) AS n FROM users").scalar()   # first column of first row -> 1
```

Highlights:

- **Parameter binding** — `?` and `?N` placeholders are escaped client-side; `bytes`
  values become `X'..'` BLOB literals.
- **`QueryResult`** — iterable/sized/truthy; `.first()`, `.scalar()`, `.column(name)`,
  `.rows`, `.columns` (true `SELECT` order). Never `None`.
- **`Row`** — a `dict` subclass with attribute access (`row.name`) and `row.blob("col")`
  to decode a BLOB column back to `bytes`.

### Minimal raw protocol

If you would rather speak the protocol directly, the framing is just a 4-byte
little-endian length plus JSON:

```python
import json, socket, struct

def call(host, port, request):
    payload = json.dumps(request).encode("utf-8")
    with socket.create_connection((host, port)) as sock:
        sock.sendall(struct.pack("<I", len(payload)) + payload)   # 4-byte LE length + body

        def recv_exactly(n):
            buf = b""
            while len(buf) < n:
                chunk = sock.recv(n - len(buf))
                if not chunk:
                    raise ConnectionError("connection closed")
                buf += chunk
            return buf

        size = struct.unpack("<I", recv_exactly(4))[0]
        return json.loads(recv_exactly(size).decode("utf-8"))

print(call("127.0.0.1", 3333, {"cmd": "LIST"}))
print(call("127.0.0.1", 3333,
           {"cmd": "QUERY", "db": "mydb", "query": "SELECT 1 AS one"}))
```

---

## Project layout

| File | Responsibility |
|------|----------------|
| `src/main.rs` | CLI/config parsing, builds the runtime, starts the server |
| `src/config.rs` | Configuration from CLI flags or a JSON config file |
| `src/server.rs` | Accept loop + `SIGINT`/`SIGTERM` graceful shutdown |
| `src/connection.rs` | Per-connection framing (4-byte length + JSON) |
| `src/handler.rs` | Request parsing, command dispatch, response building, DB cache |
| `tests/protocol.rs` | End-to-end protocol tests against the built binary |
| `build.rs` | Embeds git branch/commit for `--version` |
| `examples/python/sqlite.py` | Reference Python client (standard library only) |

---

## License

Released under the MIT License.
