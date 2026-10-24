//! The oracle and kernel tiers' evidence lines name the files a tool read
//! by `$TMP`, the per-process scratch directory, instead of repeating its
//! full path, about 70 bytes on CI, on every line (#635). The full path is
//! printed once, on the first line that needs it, so a failing call's file
//! can still be found.
//!
//! Pure string work: no tool, no fixture, no VM.

use fs_btrfs_test_support::{evidence_text, shorten_with};
use std::path::Path;

const DIR: &str = "/home/runner/work/rust-fs-btrfs/rust-fs-btrfs/tmp/fs-btrfs-tests.4242.17.0";

#[test]
fn the_scratch_directory_is_shown_as_tmp() {
    let dir = Path::new(DIR);
    assert_eq!(
        shorten_with(
            &format!("[oracle vm] btrfs check --readonly {DIR}/fs.img -> 0"),
            dir
        ),
        "[oracle vm] btrfs check --readonly $TMP/fs.img -> 0"
    );
    // Every occurrence, and the tool and its arguments untouched.
    assert_eq!(
        shorten_with(&format!("[oracle vm] cmp {DIR}/a {DIR}/b -> 1"), dir),
        "[oracle vm] cmp $TMP/a $TMP/b -> 1"
    );
    // A path outside it, an image outside it, is left as it is.
    let fixture = "[oracle vm] btrfs check /work/images/raid1.img -> 0";
    assert_eq!(shorten_with(fixture, dir), fixture);
}

#[test]
fn the_full_path_is_printed_once_with_the_first_line_that_needs_it() {
    let dir = Path::new(DIR);
    let line = format!("[kernel vm] mount -t btrfs -o ro {DIR}/fs.img -> 32");
    let first = evidence_text(&line, Some(dir), true);
    assert_eq!(
        first,
        format!("[evidence] $TMP = {DIR}\n[kernel vm] mount -t btrfs -o ro $TMP/fs.img -> 32")
    );
    assert_eq!(
        evidence_text(&line, Some(dir), false),
        "[kernel vm] mount -t btrfs -o ro $TMP/fs.img -> 32"
    );
    // A line naming no scratch file, or a process that made no scratch
    // directory, prints unchanged and owes no header.
    let fixture = "[oracle vm] btrfs check /work/images/raid1.img -> 0";
    assert_eq!(evidence_text(fixture, Some(dir), true), fixture);
    assert_eq!(evidence_text(&line, None, true), line);
}
