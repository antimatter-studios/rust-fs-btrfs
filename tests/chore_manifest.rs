//! `chore_min_version` must cover the keys `chores.yml` actually uses.
//!
//! # The failure this exists to stop
//!
//! `chore_min_version` is enforced as a floor on the running binary,
//! and chore's YAML decoder uses `KnownFields(true)` -- an unknown key
//! is a hard parse error, not a field it ignores. So a declared floor
//! that is LOWER than the keys in the file produces the worst of both:
//! a binary between the declared floor and the real one passes the
//! version check, then fails to load the file at all with a generic
//! `unknown field "timeout" in a Task`, which never mentions
//! `chore_min_version` and sends the reader looking at their YAML
//! instead of at their chore build.
//!
//! `timeout:` and `on_timeout:` were adopted here while the floor still
//! said `0.6.0`, and the identical drift happened in a sibling
//! repository in the same week. It is not the kind of mistake that gets
//! noticed by the person who makes it, because their own chore is new
//! enough.
//!
//! # What this guard does NOT do
//!
//! It is not a model of chore's schema and cannot be: nothing available
//! here knows which release introduced a given key. It is a table of
//! the keys THIS repository uses whose introducing version is known,
//! and a row is added when a newer key is adopted. So it catches the
//! mistake that has already been made twice rather than every mistake
//! of the shape. A guard that under-claims and says so is worth more
//! than one that implies coverage it has not got.

use std::path::PathBuf;

/// Keys whose presence requires a chore at least this new.
const KEYS_REQUIRING: &[(&str, Version)] = &[
    // chore ab59c04, "Add timeout:/on_timeout:, the net for a task
    // that hangs", released in 0.10.0.
    ("timeout:", (0, 10, 0)),
    ("on_timeout:", (0, 10, 0)),
];

type Version = (u64, u64, u64);

fn chores_yml() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("chores.yml")
}

/// A line with any trailing comment removed.
///
/// A line that is ONLY a comment needs no case of its own: `#` stays
/// glued to whatever follows it, so `# timeout: 45m` does not start
/// with `timeout:` and `# chore_min_version: 0.9.0` does not start
/// with `chore_min_version:`. An earlier draft rejected comment lines
/// explicitly, and the mutation deleting that branch left all eight
/// tests green -- which is what a REDUNDANT check looks like, as
/// distinct from an unwitnessed one, so it is gone rather than
/// propped up with a test written to justify it.
///
/// The trailing case is the one that bites: `chore_min_version: 0.6.0
/// # bump when adopting a newer key` parses as the version `0.6.0 #
/// bump when adopting a newer key` without this, which is no version
/// at all, and a floor that fails to parse is a floor that is not
/// checked.
fn without_comment(line: &str) -> &str {
    let t = line.trim();
    match t.split_once(" #") {
        Some((before, _)) => before.trim(),
        None => t,
    }
}

fn parse_version(raw: &str) -> Option<Version> {
    let v = raw.trim().trim_matches(|c| c == '"' || c == '\'').trim();
    let mut parts = v.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn declared_floor(text: &str) -> Option<Version> {
    text.lines()
        .map(without_comment)
        .find_map(|l| l.strip_prefix("chore_min_version:"))
        .and_then(parse_version)
}

/// Every key present in `text` that the declared floor does not cover,
/// as a sentence naming the key, what it needs and what is declared.
fn violations(text: &str) -> Vec<String> {
    let Some(declared) = declared_floor(text) else {
        return vec!["chores.yml declares no chore_min_version at all".to_string()];
    };
    let mut out = Vec::new();
    for (key, required) in KEYS_REQUIRING {
        let used = text
            .lines()
            .map(without_comment)
            .any(|l| l.starts_with(key));
        if used && declared < *required {
            out.push(format!(
                "`{key}` needs chore {}.{}.{}, chore_min_version declares {}.{}.{}",
                required.0, required.1, required.2, declared.0, declared.1, declared.2
            ));
        }
    }
    out
}

/// THE ONE THAT READS THE REAL FILE.
#[test]
fn the_declared_floor_covers_the_keys_this_repository_uses() {
    let path = chores_yml();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    assert!(
        declared_floor(&text).is_some(),
        "control: chores.yml must declare a chore_min_version, or every assertion \
         below passes for the wrong reason"
    );
    let found = violations(&text);
    assert!(
        found.is_empty(),
        "chores.yml uses keys its declared floor does not cover, so a chore between \
         the two passes the version check and then cannot parse the file at all: {found:?}"
    );
}

mod parser {
    use super::{declared_floor, violations};

    const OLD_FLOOR: &str = "\
version: \"3\"
chore_min_version: 0.6.0

tasks:
  test:
    cmds:
      - cmd: 'cargo test'
";

    #[test]
    fn a_key_newer_than_the_floor_is_reported() {
        let yaml = OLD_FLOOR.replace("      - cmd: 'cargo test'\n", "    timeout: 45m\n");
        let found = violations(&yaml);
        assert_eq!(
            found.len(),
            1,
            "expected exactly the timeout violation: {found:?}"
        );
        assert!(
            found[0].contains("0.10.0"),
            "the message must name the version needed"
        );
        assert!(
            found[0].contains("0.6.0"),
            "and the one declared, or it cannot be acted on"
        );
    }

    /// A key NAMED IN A COMMENT is not a key. `chores.yml` here
    /// discusses `timeout:` at length before using it, and a guard that
    /// counted the prose would raise the floor of a file that does not
    /// need it -- and could never be trusted to mean what it says.
    #[test]
    fn a_key_only_mentioned_in_a_comment_is_not_used() {
        let yaml = OLD_FLOOR.replace(
            "      - cmd: 'cargo test'\n",
            "    # `timeout:` is the third layer, and `on_timeout:` runs after it\n",
        );
        assert!(
            violations(&yaml).is_empty(),
            "the prose above a key is not the key"
        );
    }

    #[test]
    fn a_floor_that_covers_the_key_is_not_reported() {
        let yaml = OLD_FLOOR
            .replace("chore_min_version: 0.6.0", "chore_min_version: 0.10.0")
            .replace("      - cmd: 'cargo test'\n", "    timeout: 45m\n");
        assert!(violations(&yaml).is_empty(), "0.10.0 covers a 0.10.0 key");
    }

    #[test]
    fn a_newer_floor_than_needed_is_not_reported() {
        let yaml = OLD_FLOOR
            .replace("chore_min_version: 0.6.0", "chore_min_version: 0.11.0")
            .replace("      - cmd: 'cargo test'\n", "    on_timeout:\n");
        assert!(
            violations(&yaml).is_empty(),
            "the floor is a minimum, not an equality"
        );
    }

    #[test]
    fn a_quoted_floor_is_the_same_floor() {
        let yaml = OLD_FLOOR.replace("chore_min_version: 0.6.0", "chore_min_version: \"0.6.0\"");
        assert_eq!(
            declared_floor(&yaml),
            Some((0, 6, 0)),
            "a quoted version string is the same version -- the spelling that has \
             defeated three other comparisons in this repository"
        );
    }

    #[test]
    fn a_trailing_comment_does_not_hide_the_floor() {
        let yaml = OLD_FLOOR.replace(
            "chore_min_version: 0.6.0",
            "chore_min_version: 0.6.0 # bump when adopting a newer key",
        );
        assert_eq!(declared_floor(&yaml), Some((0, 6, 0)));
    }

    #[test]
    fn a_missing_floor_is_itself_a_violation() {
        let yaml = OLD_FLOOR.replace("chore_min_version: 0.6.0\n", "");
        assert_eq!(
            violations(&yaml).len(),
            1,
            "a file with no floor declares no floor"
        );
    }
}
