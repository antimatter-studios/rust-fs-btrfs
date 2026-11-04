//! A call's captured stdout and stderr come back as they are, or the call
//! fails. The guest's shell redirect creates both files before the tool
//! runs, so one that cannot be read after the call has a status is a fault.
//! Reading it as empty would look the same as a tool that printed nothing
//! (#762).
//!
//! Files in the scratch directory only: no tool, no fixture, no VM.

use fs_btrfs_test_support::Run;
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

const TOOL: &str = "mkfs.btrfs";

/// A call that has finished: both stream files are there, as the guest's
/// redirect leaves them.
fn finished_call(stdout: &[u8], stderr: &[u8]) -> Run {
    let run = Run::new();
    fs::create_dir_all(&run.dir).unwrap();
    fs::write(&run.stdout, stdout).unwrap();
    fs::write(&run.stderr, stderr).unwrap();
    run
}

/// Remove `stream` and read the call back: the read must fail the call, and
/// its message must name the tool, the file and the error.
fn removing_it_fails_the_call(run: &Run, stream: &Path) {
    fs::remove_file(stream).unwrap();
    let error = fs::read(stream).unwrap_err().to_string();

    let failure = catch_unwind(AssertUnwindSafe(|| run.streams(TOOL)))
        .expect_err("a stream file that cannot be read was reported as empty output");

    let message = failure
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| failure.downcast_ref::<&str>().copied())
        .expect("the failure must have a message");
    for text in [TOOL, &stream.display().to_string(), &error] {
        assert!(message.contains(text), "{message:?} does not name {text:?}");
    }
}

#[test]
fn a_removed_stdout_file_fails_the_call() {
    let run = finished_call(b"out", b"err");
    removing_it_fails_the_call(&run, &run.stdout);
}

#[test]
fn a_removed_stderr_file_fails_the_call() {
    let run = finished_call(b"out", b"err");
    removing_it_fails_the_call(&run, &run.stderr);
}

/// The other side of the same line: a tool that printed nothing still reads
/// back as empty output, and each stream keeps its own bytes.
#[test]
fn a_stream_the_tool_left_empty_reads_back_as_empty() {
    let run = finished_call(b"", b"only stderr\n");
    let (stdout, stderr) = run.streams(TOOL);
    assert_eq!(stdout, b"");
    assert_eq!(stderr, b"only stderr\n");

    let run = finished_call(b"only stdout\n", b"");
    let (stdout, stderr) = run.streams(TOOL);
    assert_eq!(stdout, b"only stdout\n");
    assert_eq!(stderr, b"");
}

/// A read that succeeds still cleans up after itself, so a long suite does
/// not collect one pair of files per call.
#[test]
fn a_read_removes_both_stream_files() {
    let run = finished_call(b"out", b"err");
    let _ = run.streams(TOOL);
    assert!(!run.stdout.exists());
    assert!(!run.stderr.exists());
}
