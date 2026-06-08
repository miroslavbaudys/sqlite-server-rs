<!--
Thanks for contributing to sqlite-server-rs! Please fill in the sections below.
See CONTRIBUTING.md for the full guidelines.
-->

## Summary

<!-- What does this change do, and why? Keep it focused on one logical change. -->

## Related issue

<!-- e.g. "Closes #123", or "N/A" -->

## Type of change

- [ ] Bug fix
- [ ] New feature
- [ ] Refactor / cleanup
- [ ] Documentation
- [ ] Build / CI

## How was this tested?

<!--
Describe how you verified the change: the commands you ran, the request
payload(s) you sent, and what you observed. Prefer adding a test in tests/.
-->

## Checklist

- [ ] `cargo build` is clean with no new warnings on the files I touched.
- [ ] `cargo test` passes.
- [ ] `cargo fmt --check` and `cargo clippy --all-targets` pass.
- [ ] Existing commands (`QUERY`, `LIST`, `DELETE_DB`) still behave as documented.
- [ ] Wire compatibility is preserved (framing, error codes, value encoding, key ordering).
- [ ] Untrusted input handling is preserved (no regression in `db`-name / path-traversal validation).
- [ ] Access control is preserved (if I touched `--auth` / `--ip-whitelist`, unauthenticated or non-whitelisted clients still never reach command handling).
- [ ] Commit messages use the imperative mood and a concise subject.
- [ ] Documentation (README / comments) is updated where relevant.
