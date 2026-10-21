# Vendored twilight-model 0.17.1

Upstream: https://crates.io/crates/twilight-model/0.17.1 (ISC license, see LICENSE.md).
Used through `[patch.crates-io]` in the root `Cargo.toml`. Patches (search for
"twilight-dispatch patch"; their tests are listed below):

1. `gateway/presence/activity.rs`: `Activity.application_id` accepts `0`, `"0"`,
   `""` and `null` as "no application" instead of failing to deserialize the
   whole presence / GUILD_CREATE.
2. `channel/flags.rs`: adds `HIDE_MEDIA_DOWNLOAD_OPTIONS` (1 << 15) and
   `CHANNEL_OBFUSCATED` (1 << 17, required from 2026-11-16) and keeps unknown
   bits (`from_bits_retain`) instead of dropping them.
3. Every flags type that is stored in the cache keeps unknown bits
   (`from_bits_retain`), so values Discord adds later survive a round trip:
   `guild/permissions.rs`, `guild/member_flags.rs`, `guild/role_flags.rs`,
   `guild/system_channel_flags.rs`, `channel/message/flags.rs`,
   `channel/attachment_flags.rs`, `gateway/presence/activity_flags.rs`,
   `user/flags.rs`, `oauth/application_flags.rs`,
   `application/monetization/sku_flags.rs`.
4. (dropped) twilight-model 0.17.1 already serializes discriminator `0` (users
   on the new username system) as `"0"`, not `"0000"`: `DiscriminatorDisplay`
   pads only 1 to 999. Still pinned by `migrated_discriminator_is_zero` in
   `src/model_patches.rs`.
5. Unicode emojis (only a name) whose id Discord sends as `0`, `"0"` or `""`,
   or leaves out, parse with no id instead of failing the whole GUILD_CREATE /
   CHANNEL_* / MESSAGE_CREATE (`invalid value: integer 0, expected non zero
   u64`): `visitor.rs` `zeroable_id` also treats `"0"` and `""` as none
   (upstream only handled the integer), and it is used for every optional
   emoji id: `channel/forum.rs` (`ForumTag`, `DefaultReaction`),
   `guild/invite/welcome_screen.rs`, `guild/onboarding/option.rs`,
   `poll/media.rs`. `gateway/presence/activity_emoji.rs`: a custom status
   emoji id may be an integer (kept as a string, as before). They are stored
   as Discord documents a unicode emoji: `"emoji_id": null` with the name.

## Tests

The vendored crates are not members of the root package's workspace, so their
own unit tests are never built and are not kept in step with the patches:
after patches 2 and 3 the `serde` tests of the patched flags files still
assert that unknown bits are dropped ("Deserialization truncates unknown
bits"). They describe upstream, not this copy. The tests of record are:

- `src/model_patches.rs` (patches 1, 2, 3 and 5, and the upstream behaviour
  item 4 relied on),
- `tests/guards.rs`: `vendored_flags_keep_unknown_bits` fails when a flags
  type's `Deserialize` drops unknown bits again (patches 2 and 3),
- `tests/emoji.rs` (patch 5, through the cache),
- `tests/repo_files.rs`: `added_channel_flags_have_discords_bits` pins the
  bits of patch 2's flags (nothing else uses `HIDE_MEDIA_DOWNLOAD_OPTIONS`),
  `the_vendored_twilight_crates_are_the_ones_built` fails when
  `[patch.crates-io]` stops applying (cargo only warns),
  `every_patch_hunk_carries_the_marker` when a hunk lost its marker and
  `patches_md_names_what_exists` when this file names a file, flag or test
  that does not exist.

To upgrade: copy the new release over this directory, re-apply the patches,
and run `cargo test` in the repository root.
