# Contributing to sqlite-server-rs

Thanks for your interest in improving `sqlite-server-rs`! This document explains
how to build the project, the conventions we follow, and how to get changes merged.

By participating you agree to keep interactions respectful and constructive (see
[`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md)).

## Ways to contribute

- **Report a bug** — open a [GitHub issue](../../issues) with steps to reproduce.
- **Suggest a feature** — open an issue describing the use case before sending a
  large change, so we can agree on the approach.
- **Send a fix or improvement** — open a pull request (see below).

> **Security issues:** please do **not** open a public issue. Follow
> [`SECURITY.md`](SECURITY.md) and report privately via GitHub Security Advisories.

## Reporting bugs

A good report includes:

- The version or commit hash you are on.
- Your OS and Rust toolchain version (`rustc --version`, `cargo --version`).
- The exact request that triggers it — the `cmd` / `db` / `query` payload — and
  the response or crash you observed versus what you expected.
- A minimal reproduction if possible.

## Development setup

You need a recent stable Rust toolchain (`cargo`/`rustc`). SQLite is compiled from
source via `rusqlite`'s `bundled` feature, so there is no system SQLite dependency.

```sh
git clone https://github.com/miroslavbaudys/sqlite-server-rs.git
cd sqlite-server-rs
cargo build          # first run also compiles bundled SQLite
```

## Running and testing your change

The project ships an end-to-end test suite that boots the built binary and speaks
the protocol over a real socket:

```sh
cargo test           # runs tests/protocol.rs
cargo fmt --check    # formatting
cargo clippy --all-targets   # lints
```

To exercise it manually, run the server and send requests with the reference
client in [`examples/python/sqlite.py`](examples/python/sqlite.py) or the
raw-protocol snippet in the [README](README.md#python-client):

```sh
mkdir -p /tmp/sqlite-data
cargo run -- --ip 127.0.0.1 --port 3333 --databases-folder /tmp/sqlite-data
```

Before opening a PR, please confirm:

- `cargo build` is **clean** (no new warnings on the files you touched).
- `cargo test`, `cargo fmt --check`, and `cargo clippy --all-targets` all pass.
- Existing commands (`QUERY`, `LIST`, `DELETE_DB`) still behave as documented.
- New behaviour is covered by a test in `tests/` where practical.

## Coding guidelines

- **Language:** Rust (stable, 2021 edition). Prefer the standard library and the
  existing crates over adding new dependencies; discuss new deps in an issue first.
- **Wire compatibility:** the protocol is shared with the original C++
  `sqlite-server`. Do not change framing, error codes, value encoding, or key
  ordering without a deliberate, documented reason — existing clients depend on it.
- **Match the surrounding style** — formatting is enforced by `rustfmt`; keep
  naming and comment density consistent with the file you are editing.
- **Error handling:** surface failures as error responses, never by panicking on a
  malformed request. A hostile client must not be able to take down the server.
- **Untrusted input:** treat the `db` name and `query` as hostile. Preserve the
  existing path-traversal and validation guarantees, and add tests when you touch
  them.
- **Keep diffs focused** — one logical change per pull request. Unrelated
  formatting churn makes review harder.
- Do not commit build artifacts; `target/` is ignored.

## Commit messages

- Use the **imperative mood** in the subject: "Add …", "Fix …", "Refactor …".
- Keep the subject concise (≈ 72 characters) and capitalized, with no trailing period.
- Add a body when the *why* isn't obvious from the subject; wrap it at ~72 columns.
- Group related changes into a single, coherent commit.

## Pull request process

1. Fork the repository and create a topic branch off `main`
   (e.g. `fix/blob-encoding`).
2. Make your change, following the guidelines above.
3. Ensure `cargo test`, `cargo fmt --check`, and `cargo clippy` pass.
4. Open a pull request with a clear description of **what** changed and **why**,
   and link any related issue.
5. Be responsive to review feedback; keep the branch up to date with `main`.

## License

By contributing, you agree that your contributions will be licensed under the
project's [MIT License](LICENSE).
