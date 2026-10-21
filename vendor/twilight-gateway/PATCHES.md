# Vendored twilight-gateway 0.17.1

Upstream: https://crates.io/crates/twilight-gateway/0.17.1 (ISC license, see LICENSE.md).
Used through `[patch.crates-io]` in the root `Cargo.toml`. Patches (search for
"twilight-dispatch patch"):

1. `src/shard.rs` `disconnect()`: drop the session and resume URL when the
   gateway closes with 4007 (invalid sequence) or 4009 (session timed out).
   Discord documents both as "reconnect and start a new session"; unpatched,
   the shard keeps sending RESUME for the dead session forever.

Tested in the repository root: `tests/e2e.rs`
(`close_4007_invalid_seq_starts_a_new_session`,
`close_4009_session_timed_out_starts_a_new_session`) and `tests/runner.rs`
(`the_session_is_cleared_when_twilight_drops_it`) close after READY;
`tests/repo_gateway_resume_close.rs` covers Discord's usual case, a RESUME of
a restored session answered with 4007 or 4009
(`a_resume_answered_with_4007_identifies`,
`a_resume_answered_with_4009_identifies`), and that a resumable close still
resumes (`a_resume_answered_with_4000_resumes_again`). `tests/repo_files.rs`
checks that the patch keeps its marker and that the vendored crates are the
ones built. The vendored crate's own unit tests are never built (it is not a
workspace member).

To upgrade: copy the new release over this directory, re-apply the patch, and
run `cargo test` in the repository root.
