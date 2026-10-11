//! Every guest script reads each pipe it opens to the end.
//!
//! The kernel oracle runs a test's script in the harness VM under
//! `bash -euo pipefail`. A reader that stops early -- `head -n N`, or an
//! `awk` program that calls `exit` -- closes its end of the pipe while the
//! writer may still have output to send; the writer then dies of SIGPIPE,
//! exit 141, and `pipefail` makes the whole script fail. Whether the writer
//! is still writing depends on how its output is buffered and scheduled,
//! so the failure comes and goes: merge-queue run 38110611259 lost
//! `block_group_used` to `[kernel vm] mount ... -> 141` on a tree that
//! passed everywhere else (#279).
//!
//! Read every line and select with `awk 'NR <= N'`, or match the one line
//! wanted without `exit`.

use std::path::Path;

/// `(file, line)` of every early-exiting reader in a test's source.
fn early_readers(name: &str, source: &str) -> Vec<String> {
    source
        .lines()
        .enumerate()
        .filter(|(_, l)| l.contains("| head ") || l.contains("| head\"") || l.contains("; exit}"))
        .map(|(i, l)| format!("{name}:{}: {}", i + 1, l.trim()))
        .collect()
}

#[test]
fn no_guest_script_stops_reading_a_pipe_early() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let me = Path::new(file!()).file_name().unwrap();
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "rs") && path.file_name() != Some(me) {
            let source = std::fs::read_to_string(&path).unwrap();
            found.extend(early_readers(
                &path.file_name().unwrap().to_string_lossy(),
                &source,
            ));
        }
    }
    assert!(
        found.is_empty(),
        "these readers stop early, so under pipefail the writer before them can die \
         of SIGPIPE (exit 141) and fail the script at random:\n{}",
        found.join("\n")
    );
}

#[test]
fn the_reader_finds_what_it_is_meant_to() {
    assert_eq!(early_readers("x", "a | head -n 3 | b").len(), 1);
    assert_eq!(
        early_readers("x", "awk '$1 == \"0:\" {print $4; exit}'").len(),
        1
    );
    assert!(early_readers("x", "awk 'NR <= 128 && NR % 2 == 1'").is_empty());
    assert!(early_readers("x", "awk '$1 == \"0:\" {print $4}'").is_empty());
}
