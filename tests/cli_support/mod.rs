//! What the command-line tests share: the multi-call binary, reached
//! under each of its names, and a place to put scratch files.
//!
//! The binary is built only with the `cli` feature. `scripts/test.sh`
//! turns it on for every tier; a bare `cargo test` does not, and then
//! these tests FAIL naming the fix rather than skipping.

#![allow(dead_code)]

use fs_btrfs::chunk::DiskKey;
use fs_btrfs::Filesystem;
use fs_core::FileDevice;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, OnceLock};

/// The repository-named entry point, as cargo built it.
///
/// `option_env!`, not `env!`: without the feature `env!` would fail the
/// compile of every test target in the run, where this fails only the
/// tests that need the binary, each with the fix in its message.
const BIN: Option<&str> = option_env!("CARGO_BIN_EXE_rust-fs-btrfs");

const NO_BIN: &str = "the rust-fs-btrfs binary is built only with `--features cli`. Run the \
    tests through scripts/test.sh, which passes it, or add `--features cli` to cargo test.";

pub fn bin() -> &'static str {
    BIN.expect(NO_BIN)
}

/// The binary under its own (cargo's) name: the repository entry point.
pub fn entry() -> Command {
    Command::new(BIN.expect(NO_BIN))
}

/// The program as a user runs it under `name`: argv[0] is what an
/// installed symlink hands it, and what it dispatches on.
pub fn tool(name: &str) -> Command {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(BIN.expect(NO_BIN));
    cmd.arg0(name);
    cmd
}

/// A fresh scratch directory for this process, under the repository's
/// tmp/ like every other test's.
pub fn scratch_dir(tag: &str) -> PathBuf {
    let dir = PathBuf::from(fs_btrfs_test_support::temp_path!(
        "cli-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("create {}: {e}", dir.display()));
    dir
}

/// A directory holding the binary under every name it answers to, as an
/// install links it: `rust-fs-btrfs` and each dotted name, symlinks to
/// cargo's build.
pub fn names_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = scratch_dir("names");
        let mut names = dotted_names();
        names.push("rust-fs-btrfs".to_string());
        for name in names {
            std::os::unix::fs::symlink(bin(), dir.join(&name))
                .unwrap_or_else(|e| panic!("link {name}: {e}"));
        }
        dir
    })
}

/// The dotted names, as the binary itself lists them for packaging.
pub fn dotted_names() -> Vec<String> {
    let out = entry()
        .args(["generate", "names"])
        .output()
        .expect("run rust-fs-btrfs generate names");
    assert!(out.status.success(), "generate names failed: {out:?}");
    String::from_utf8(out.stdout)
        .expect("names are UTF-8")
        .lines()
        .map(str::to_string)
        .collect()
}

/// The exit status of a namespace verb's refusal, from the reason the
/// kernel gave the same call: 4, `refused`, where the volume's state
/// forbids it (a directory that is not empty, a name already there, a
/// missing target, a part of the path that is not a directory), as the
/// CLI contract has it (EXIT_REFUSED); 1 for every other refusal.
pub fn refusal_code(kernel_reason: &str) -> i32 {
    match kernel_reason {
        "Directory not empty" | "File exists" | "No such file or directory" | "Not a directory" => {
            4
        }
        _ => 1,
    }
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Run and require success, returning the output.
#[track_caller]
pub fn ok(cmd: &mut Command) -> Output {
    let out = cmd.output().expect("spawn");
    assert!(
        out.status.success(),
        "{cmd:?} failed ({:?})\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        stdout(&out),
        stderr(&out)
    );
    out
}

/// Run and require exit status `code`, with nothing on stdout and a
/// structured error on stderr; returns the error's message.
#[track_caller]
pub fn refused(cmd: &mut Command, code: i32) -> String {
    let out = cmd.output().expect("spawn");
    assert_eq!(
        out.status.code(),
        Some(code),
        "{cmd:?}\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        out.stdout.is_empty(),
        "{cmd:?} printed on stdout: {}",
        stdout(&out)
    );
    let err = stderr(&out);
    let line = err.trim_end();
    assert!(
        line.starts_with("{\"error\": \"") && line.ends_with(&format!("\"code\": {code}}}")),
        "{cmd:?}: not a structured error with code {code}: {err}"
    );
    json_field(line, "error")
}

/// The value of `"key": ...` in a JSON report: enough of a reader for the
/// flat reports these tests check, without a JSON dependency. Strings
/// come back without their quotes (escaped quotes are not unescaped);
/// anything else as written.
#[track_caller]
pub fn json_field(json: &str, key: &str) -> String {
    let needle = format!("\"{key}\": ");
    let start = json
        .find(&needle)
        .unwrap_or_else(|| panic!("no {key:?} in:\n{json}"))
        + needle.len();
    let rest = &json[start..];
    if let Some(stripped) = rest.strip_prefix('"') {
        let mut end = 0;
        let bytes = stripped.as_bytes();
        while end < bytes.len() && !(bytes[end] == b'"' && (end == 0 || bytes[end - 1] != b'\\')) {
            end += 1;
        }
        stripped[..end].to_string()
    } else {
        rest.split([',', '\n', '}'])
            .next()
            .unwrap()
            .trim()
            .to_string()
    }
}

/// The size of a tree block's header: item offsets count from its end.
const TREE_BLOCK_HEADER: usize = 101;

/// The root of tree `tree` in `fs`.
pub fn root_of(fs: &Filesystem, tree: u64) -> u64 {
    match tree {
        1 => fs.superblock().root,
        3 => fs.superblock().chunk_root,
        _ => fs
            .root_tree_items()
            .unwrap()
            .into_iter()
            .find(|(objectid, key_type, _, _)| *objectid == tree && *key_type == 132)
            .map(|(_, _, _, data)| u64::from_le_bytes(data[176..184].try_into().unwrap()))
            .unwrap_or_else(|| panic!("no root item for tree {tree}")),
    }
}

/// Change the first item of `tree` that `pick` chooses, with `edit`, and
/// write its block back with a fresh checksum to every mirror.
pub fn edit_item(
    image: &Path,
    tree: u64,
    pick: impl Fn(&DiskKey, &[u8]) -> bool,
    edit: impl Fn(&mut [u8]),
) {
    let fs =
        Filesystem::mount(Arc::new(FileDevice::open(image.to_str().unwrap()).unwrap())).unwrap();
    let mut stack = vec![root_of(&fs, tree)];
    while let Some(logical) = stack.pop() {
        let block = fs.read_tree_block(logical).unwrap();
        match &block.body {
            fs_btrfs::btree::Body::Node(ptrs) => stack.extend(ptrs.iter().map(|p| p.blockptr)),
            fs_btrfs::btree::Body::Leaf(items) => {
                for item in items {
                    let data = block.item_data(item).unwrap();
                    if !pick(&item.key, data) {
                        continue;
                    }
                    let mut bytes = block.bytes().to_vec();
                    let at = TREE_BLOCK_HEADER + item.offset as usize;
                    edit(&mut bytes[at..at + item.size as usize]);
                    fs_btrfs::tree_write::stamp_checksum(&mut bytes, fs.superblock());
                    let file = std::fs::OpenOptions::new().write(true).open(image).unwrap();
                    for mirror in 0..fs.chunk_map().mirrors_at(logical).unwrap() {
                        let m = fs.chunk_map().map_mirror(logical, mirror).unwrap();
                        file.write_all_at(&bytes, m.physical).unwrap();
                    }
                    return;
                }
            }
        }
    }
    panic!("no item in tree {tree} matched");
}
