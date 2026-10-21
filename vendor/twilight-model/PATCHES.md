# Vendored twilight-model 0.17.1

Upstream: https://crates.io/crates/twilight-model/0.17.1 (ISC license, see LICENSE.md).
Used through `[patch.crates-io]` in the root `Cargo.toml`. Patches (search for
"twilight-dispatch patch"); they are tested in `src/model_patches.rs` of
twilight-dispatch:

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
4. `user/mod.rs`: discriminator `0` (users on the new username system) is
   serialized as `"0"`, not `"0000"`.
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

To upgrade: copy the new release over this directory, re-apply the patches,
and run `cargo test` in the repository root.
