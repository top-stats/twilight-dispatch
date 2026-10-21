//! The repository's own files, checked where no build step does:
//! - the vendored twilight crates: `[patch.crates-io]` builds them, every
//!   patch hunk carries the marker their PATCHES.md says to search for, and
//!   each PATCHES.md names files and tests that exist;
//! - the bits of the channel flags the vendored twilight-model adds;
//! - one rustls for twilight and the service (main() installs the crypto
//!   provider twilight's TLS connector needs);
//! - CI: actions pinned to commits and kept current by Dependabot, checkouts
//!   that do not keep the token, no default token permissions and a timeout
//!   on every job.
//!
//! Only file reads and serde: no services, so these always run.

use serde_json::json;
use std::path::{Path, PathBuf};
use twilight_model::channel::ChannelFlags;

/// What both PATCHES.md say to search for to find the patches.
const MARKER: &str = "twilight-dispatch patch";
const CHANNEL_FLAGS: &str = "vendor/twilight-model/src/channel/flags.rs";
const SHARD: &str = "vendor/twilight-gateway/src/shard.rs";

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn read_repo_file(path: impl AsRef<Path>) -> String {
    let path = root().join(path);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

// ---------------------------------------------------------------------------
// Vendored twilight crates

/// Whether the comment right above `lines[index]` carries the marker.
fn marked(lines: &[&str], index: usize) -> bool {
    lines[..index]
        .iter()
        .rev()
        .take_while(|line| line.trim_start().starts_with("//"))
        .any(|line| line.contains(MARKER))
}

/// The text between backticks in a Markdown file.
fn code_spans(markdown: &str) -> Vec<&str> {
    markdown.split('`').skip(1).step_by(2).collect()
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// A name in code (`from_bits_retain`, `CHANNEL_OBFUSCATED`, a test), not a
/// path, a value or a type.
fn is_name(span: &str) -> bool {
    span.contains('_') && span.chars().all(is_name_char)
}

/// Whether `text` contains `name` as a whole name.
fn contains_name(text: &str, name: &str) -> bool {
    text.match_indices(name).any(|(at, _)| {
        !text[..at].ends_with(is_name_char) && !text[at + name.len()..].starts_with(is_name_char)
    })
}

/// Patch 2 of vendor/twilight-model: flags twilight 0.17.1 does not define.
/// Only this pins their bits: a typo or a hunk lost on upgrade would go
/// unnoticed.
#[test]
fn added_channel_flags_have_discords_bits() {
    assert_eq!(ChannelFlags::HIDE_MEDIA_DOWNLOAD_OPTIONS.bits(), 1 << 15);
    assert_eq!(ChannelFlags::CHANNEL_OBFUSCATED.bits(), 1 << 17);
    let flags: ChannelFlags = serde_json::from_value(json!(1u64 << 15)).unwrap();
    assert_eq!(flags, ChannelFlags::HIDE_MEDIA_DOWNLOAD_OPTIONS);
    assert_eq!(serde_json::to_value(flags).unwrap(), json!(32_768));
}

/// Both PATCHES.md tell whoever upgrades twilight to find the patches by
/// searching for the marker: a hunk without it is lost on the next upgrade.
#[test]
fn every_patch_hunk_carries_the_marker() {
    let mut files = Vec::new();
    rust_files(&root().join("vendor/twilight-model/src"), &mut files);
    let mut retains = 0;
    let mut unmarked = Vec::new();
    for file in &files {
        let source = std::fs::read_to_string(file).unwrap();
        let lines: Vec<&str> = source.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            // Patches 2 and 3: upstream drops the bits it does not know.
            if line.contains("from_bits_retain") {
                retains += 1;
                if !marked(&lines, index) {
                    unmarked.push(format!("{}:{}", file.display(), index + 1));
                }
            }
        }
    }
    assert!(
        retains >= 11,
        "only {retains} from_bits_retain: scanner out of date?"
    );
    for (path, needle) in [
        (CHANNEL_FLAGS, "const HIDE_MEDIA_DOWNLOAD_OPTIONS "),
        (CHANNEL_FLAGS, "const CHANNEL_OBFUSCATED "),
        (SHARD, "CloseInitiator::Gateway(Some(4007 | 4009))"),
    ] {
        let source = read_repo_file(path);
        let lines: Vec<&str> = source.lines().collect();
        let index = lines
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("{path}: no `{needle}`"));
        if !marked(&lines, index) {
            unmarked.push(format!("{path}:{}", index + 1));
        }
    }
    assert!(
        unmarked.is_empty(),
        "no \"{MARKER}\" comment above: {unmarked:#?}"
    );
}

/// Each PATCHES.md is the checklist for the next twilight upgrade: the files
/// it names exist, the vendored ones carry the marker, and every name in it
/// (a flag, a function, a test of record) is in one of those files, so a
/// renamed test does not leave the list stale.
#[test]
fn patches_md_names_what_exists() {
    for vendored in ["vendor/twilight-model", "vendor/twilight-gateway"] {
        let dir = root().join(vendored);
        // `channel/flags.rs` is in the crate's src/, `src/shard.rs` in the
        // crate and `tests/guards.rs` in this repository.
        let bases = [dir.join("src"), dir.clone(), root().to_path_buf()];
        let doc = read_repo_file(format!("{vendored}/PATCHES.md"));
        let spans = code_spans(&doc);
        let mut sources = Vec::new();
        for span in spans.iter().filter(|span| span.ends_with(".rs")) {
            let path = bases
                .iter()
                .map(|base| base.join(span))
                .find(|path| path.is_file())
                .unwrap_or_else(|| panic!("{vendored}/PATCHES.md: no file `{span}`"));
            let source = std::fs::read_to_string(&path).unwrap();
            if path.starts_with(&dir) {
                assert!(
                    source.contains(MARKER),
                    "{}: patched, but no \"{MARKER}\" in it",
                    path.display()
                );
            }
            sources.push(source);
        }
        assert!(!sources.is_empty(), "{vendored}/PATCHES.md: no files");
        for span in spans {
            if is_name(span) {
                assert!(
                    sources.iter().any(|source| contains_name(source, span)),
                    "{vendored}/PATCHES.md: `{span}` is in none of the files it names"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cargo.lock

/// The `[[package]]` entries of Cargo.lock for the crate `name`.
fn lock_entries<'a>(lockfile: &'a str, name: &str) -> Vec<&'a str> {
    let header = format!("name = \"{name}\"");
    lockfile
        .split("[[package]]")
        .filter(|entry| entry.lines().any(|line| line == header))
        .collect()
}

/// `[patch.crates-io]` makes cargo build the vendored, patched twilight-model
/// and twilight-gateway. When Cargo.toml asks for a version a copy does not
/// have, cargo builds the crates.io release instead with only a warning, and
/// the patches silently stop applying.
#[test]
fn the_vendored_twilight_crates_are_the_ones_built() {
    let lockfile = read_repo_file("Cargo.lock");
    assert!(
        !lockfile.contains("[[patch.unused]]"),
        "a [patch.crates-io] entry is not used"
    );
    for name in ["twilight-model", "twilight-gateway"] {
        let entries = lock_entries(&lockfile, name);
        assert_eq!(entries.len(), 1, "{name} is built once: {entries:#?}");
        assert!(
            !entries[0].contains("\nsource = "),
            "{name} comes from a registry, not vendor/{name}: {entries:#?}"
        );
    }
}

/// main() installs ring as the process-wide crypto provider of this crate's
/// rustls, and twilight's TLS connector panics when its rustls has none, so
/// both must be the same crate (Dependabot keeps rustls within its minor
/// release).
#[test]
fn twilight_and_the_service_share_one_rustls() {
    let lockfile = read_repo_file("Cargo.lock");
    let entries = lock_entries(&lockfile, "rustls");
    assert_eq!(entries.len(), 1, "rustls is built once: {entries:#?}");
}

// ---------------------------------------------------------------------------
// CI

/// Leading spaces.
fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Every workflow under .github/workflows, with its text.
fn workflows() -> Vec<(PathBuf, String)> {
    let dir = root().join(".github/workflows");
    let mut workflows = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let extension = path.extension().and_then(|e| e.to_str());
        if matches!(extension, Some("yml" | "yaml")) {
            let text = std::fs::read_to_string(&path).unwrap();
            workflows.push((path, text));
        }
    }
    assert!(!workflows.is_empty(), "no workflows");
    workflows
}

/// The action of a `uses:` line (`owner/repo@ref # version`).
fn uses(line: &str) -> Option<&str> {
    let key = line.trim_start().trim_start_matches("- ");
    key.strip_prefix("uses: ")
}

/// Whether an action reference names a commit, which a moved tag cannot
/// change.
fn pinned(action: &str) -> bool {
    let reference = action.split_once('@').map_or("", |(_, rest)| rest);
    let commit = reference.split_whitespace().next().unwrap_or_default();
    commit.len() == 40 && commit.chars().all(|c| c.is_ascii_hexdigit())
}

/// The lines of the step `lines[index]` is in: from its `- ` line to the next
/// line that is not indented further.
fn step<'a>(lines: &'a [&'a str], index: usize) -> &'a [&'a str] {
    let start = lines[..=index]
        .iter()
        .rposition(|line| line.trim_start().starts_with("- "))
        .expect("a step");
    let depth = indent(lines[start]);
    let end = lines[start + 1..]
        .iter()
        .position(|line| !line.trim().is_empty() && indent(line) <= depth)
        .map_or(lines.len(), |offset| start + 1 + offset);
    &lines[start..end]
}

/// Whether one of `lines`, trimmed, is `text`.
fn has_line(lines: &[&str], text: &str) -> bool {
    lines.iter().any(|line| line.trim() == text)
}

/// Actions are pinned to commits, so a moved tag cannot change what runs, and
/// Dependabot keeps the pins current. Checkouts do not leave the token in
/// .git/config: the image job runs on a persistent self-hosted runner.
#[test]
fn ci_actions_are_pinned_and_checkouts_keep_no_token() {
    let dependabot = read_repo_file(".github/dependabot.yml");
    assert!(
        dependabot.contains("package-ecosystem: github-actions"),
        "Dependabot does not update the pinned actions"
    );
    let mut actions = 0;
    for (path, workflow) in workflows() {
        let lines: Vec<&str> = workflow.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            if let Some(action) = uses(line) {
                actions += 1;
                let at = format!("{}:{}", path.display(), index + 1);
                assert!(
                    pinned(action) || action.starts_with("./"),
                    "{at}: `{action}` is not pinned to a commit"
                );
                if action.starts_with("actions/checkout@") {
                    assert!(
                        has_line(step(&lines, index), "persist-credentials: false"),
                        "{at}: the checkout keeps the token in .git/config"
                    );
                }
            }
        }
    }
    assert!(actions > 0, "no actions found");
}

/// The token has no permissions a job does not ask for, and no job can hold a
/// runner for GitHub's default of 6 hours.
#[test]
fn ci_token_has_no_default_permissions_and_every_job_times_out() {
    for (path, workflow) in workflows() {
        let lines: Vec<&str> = workflow.lines().collect();
        assert!(
            lines.iter().any(|line| line.starts_with("permissions:")),
            "{}: no workflow-level permissions (permissions: {{}})",
            path.display()
        );
        let start = lines
            .iter()
            .position(|line| *line == "jobs:")
            .unwrap_or_else(|| panic!("{}: no jobs", path.display()));
        let mut jobs: Vec<&str> = Vec::new();
        let mut timed: Vec<&str> = Vec::new();
        for line in &lines[start + 1..] {
            let key = line.trim();
            if key.is_empty() || key.starts_with('#') {
                continue;
            }
            match indent(line) {
                0 => break,
                2 => jobs.push(key),
                4 if key.starts_with("timeout-minutes:") => timed.extend(jobs.last().copied()),
                _ => {}
            }
        }
        assert!(!jobs.is_empty(), "{}: no jobs", path.display());
        jobs.retain(|job| !timed.contains(job));
        assert!(
            jobs.is_empty(),
            "{}: no timeout-minutes in {jobs:?}",
            path.display()
        );
    }
}
