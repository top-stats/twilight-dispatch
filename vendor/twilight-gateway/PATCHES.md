# Vendored twilight-gateway 0.17.1

Upstream: https://crates.io/crates/twilight-gateway/0.17.1 (ISC license, see LICENSE.md).
Used through `[patch.crates-io]` in the root `Cargo.toml`. Patches (search for
"twilight-dispatch patch"):

1. `src/shard.rs` `disconnect()`: drop the session and resume URL when the
   gateway closes with 4007 (invalid sequence) or 4009 (session timed out).
   Discord documents both as "reconnect and start a new session"; unpatched,
   the shard keeps sending RESUME for the dead session forever.

To upgrade: copy the new release over this directory, re-apply the patch, and
run `cargo test` in the repository root.
