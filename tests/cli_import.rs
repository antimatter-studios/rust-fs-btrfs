//! `fs.btrfs import` (#431): a local file, stdin or a local tree streamed
//! into the image through rust-fs-core's shared import, on this crate's
//! own `mkfs.btrfs` volume.
//!
//! The file is staged under a private name beside the destination,
//! written in bounded chunks, read back through a fresh open for
//! `--verify`, and published by a rename only when it is complete. Every
//! failure here -- a refusal, a volume out of space, a signal -- must
//! leave the destination as it was and no staged name behind. What the
//! kernel and `btrfs check` make of an imported file is
//! tests/cli_import_kernel.rs.
//!
//! No fixture, no VM: the unit tier.

mod cli_support;

use cli_support::*;
use fs_btrfs::Filesystem;
use fs_btrfs_test_support::sha256_hex;
use fs_core::{BlockDevice, FileDevice};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The prefix every staged name starts with.
const STAGED: &str = ".fs-btrfs-import-";

/// Bytes nobody would type: a fixed LCG, so a failure reproduces.
fn pattern(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    (0..len)
        .map(|_| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (x >> 16) as u8
        })
        .collect()
}

/// A fresh 128 MiB volume made by this crate's `mkfs.btrfs`: one 8 MiB
/// data block group, which the out-of-space test fills.
fn volume(tag: &str) -> PathBuf {
    let img = scratch_dir(tag).join("import.img");
    ok(tool("mkfs.btrfs")
        .args(["-q", "--text", "--size", "128M"])
        .arg(&img));
    img
}

fn mount(img: &Path) -> Filesystem {
    let dev = Arc::new(FileDevice::open(img).unwrap());
    Filesystem::mount(dev as Arc<dyn fs_core::BlockRead>).unwrap()
}

fn mount_rw(img: &Path) -> Filesystem {
    let dev = Arc::new(FileDevice::open_rw(img).unwrap());
    Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).unwrap()
}

/// The bytes of `path` in the image, read by the library.
fn read(img: &Path, path: &str) -> Vec<u8> {
    let fs = mount(img);
    let ino = fs.lookup_path(path).unwrap().ino;
    fs.read_file(ino).unwrap()
}

/// Whether `path` names anything in the image.
fn exists(img: &Path, path: &str) -> bool {
    mount(img).lookup_path(path).is_ok()
}

/// The staged names left in directory `dir` of the image.
fn staged(img: &Path, dir: &str) -> Vec<String> {
    let fs = mount(img);
    let ino = fs.lookup_path(dir).unwrap().ino;
    fs.read_dir(ino)
        .unwrap()
        .into_iter()
        .map(|e| String::from_utf8_lossy(&e.name).into_owned())
        .filter(|n| n.starts_with(STAGED))
        .collect()
}

/// Whether a staged name is in the root yet. The import commits while
/// this reads, so a read that meets a half-written generation is "not yet",
/// never a failure.
fn staging(img: &Path) -> bool {
    let Ok(dev) = FileDevice::open(img) else {
        return false;
    };
    let Ok(fs) = Filesystem::mount(Arc::new(dev) as Arc<dyn fs_core::BlockRead>) else {
        return false;
    };
    let Ok(root) = fs.lookup_path("/") else {
        return false;
    };
    fs.read_dir(root.ino).is_ok_and(|entries| {
        entries
            .iter()
            .any(|e| e.name.starts_with(STAGED.as_bytes()))
    })
}

fn source(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

fn fs_btrfs(img: &Path) -> std::process::Command {
    let mut cmd = tool("fs.btrfs");
    cmd.arg(img);
    cmd
}

#[test]
fn import_help_and_man_page_describe_a_bounded_complete_copy() {
    let help = stdout(&ok(tool("fs.btrfs").args([
        "absent.img",
        "import",
        "--help",
    ])));
    for text in [
        "import notes.txt /notes.txt",
        "--replace",
        "--verify",
        "--recursive",
        "--max-depth",
        "SHA-256",
        "staged",
        "rename",
    ] {
        assert!(help.contains(text), "import help omits {text}: {help}");
    }
    let share = scratch_dir("import-docs");
    ok(entry().args(["generate", "man"]).arg(&share));
    let page = std::fs::read_to_string(share.join("man/man1/fs.btrfs-import.1"))
        .expect("a man page for fs.btrfs import")
        .replace("\\-", "-");
    for text in ["--replace", "--verify", "--recursive", "SHA-256"] {
        assert!(page.contains(text), "import man page omits {text}");
    }
}

#[test]
fn a_file_and_stdin_are_imported_whole_and_report_their_digest() {
    let img = volume("import-file");
    let dir = img.parent().unwrap().to_path_buf();
    ok(fs_btrfs(&img).args(["mkdir", "/dir"]));

    let one = source(&dir, "one", b"z");
    let out = ok(fs_btrfs(&img).arg("import").arg(&one).arg("/one.bin"));
    assert_eq!(json_field(&stdout(&out), "sha256"), sha256_hex(b"z"));
    assert_eq!(json_field(&stdout(&out), "bytes"), "1");
    assert_eq!(json_field(&stdout(&out), "created"), "true");

    // More than one chunk and not a whole number of sectors.
    let big = pattern((3 << 20) + 123, 431);
    let big_path = source(&dir, "big", &big);
    let out = ok(fs_btrfs(&img)
        .args(["import", "--verify"])
        .arg(&big_path)
        .arg("/dir/big.bin"));
    assert_eq!(json_field(&stdout(&out), "sha256"), sha256_hex(&big));
    assert_eq!(json_field(&stdout(&out), "verified"), "true");

    let piped = pattern(70_000, 432);
    let mut child = fs_btrfs(&img)
        .args(["import", "-", "/stdin.bin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&piped).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "import from stdin: {}", stderr(&out));
    assert_eq!(json_field(&stdout(&out), "sha256"), sha256_hex(&piped));

    assert_eq!(read(&img, "/one.bin"), b"z");
    assert_eq!(read(&img, "/dir/big.bin"), big);
    assert_eq!(read(&img, "/stdin.bin"), piped);
    assert!(staged(&img, "/").is_empty(), "{:?}", staged(&img, "/"));
    assert!(
        staged(&img, "/dir").is_empty(),
        "{:?}",
        staged(&img, "/dir")
    );
}

#[test]
fn an_existing_name_is_refused_unless_replace_is_given() {
    let img = volume("import-replace");
    let dir = img.parent().unwrap().to_path_buf();
    let old = pattern(50_000, 1);
    let new = pattern(90_000, 2);
    let old_path = source(&dir, "old", &old);
    let new_path = source(&dir, "new", &new);
    ok(fs_btrfs(&img).arg("import").arg(&old_path).arg("/f"));

    let before = std::fs::read(&img).unwrap();
    let why = refused(fs_btrfs(&img).arg("import").arg(&new_path).arg("/f"), 1);
    assert!(why.contains("--replace"), "{why}");
    assert!(
        std::fs::read(&img).unwrap() == before,
        "a refused import changed the image"
    );

    let out = ok(fs_btrfs(&img)
        .args(["import", "--replace"])
        .arg(&new_path)
        .arg("/f"));
    assert_eq!(json_field(&stdout(&out), "replaced"), "true");
    assert_eq!(read(&img, "/f"), new);
    assert!(staged(&img, "/").is_empty(), "{:?}", staged(&img, "/"));
}

#[test]
fn a_refused_import_leaves_the_image_byte_identical() {
    let img = volume("import-refused");
    let dir = img.parent().unwrap().to_path_buf();
    ok(fs_btrfs(&img).args(["mkdir", "/dir"]));
    let src = source(&dir, "src", b"payload");
    let before = std::fs::read(&img).unwrap();

    // No such parent; a parent that is a file; a directory to replace.
    ok(fs_btrfs(&img).arg("import").arg(&src).arg("/file"));
    let with_file = std::fs::read(&img).unwrap();
    assert!(refused(fs_btrfs(&img).arg("import").arg(&src).arg("/absent/f"), 4).contains("absent"));
    refused(fs_btrfs(&img).arg("import").arg(&src).arg("/file/f"), 4);
    let why = refused(
        fs_btrfs(&img)
            .args(["import", "--replace"])
            .arg(&src)
            .arg("/dir"),
        3,
    );
    assert!(why.contains("not a regular file"), "{why}");
    // A name Btrfs cannot hold.
    let long = format!("/{}", "n".repeat(256));
    refused(fs_btrfs(&img).arg("import").arg(&src).arg(&long), 3);
    assert!(
        std::fs::read(&img).unwrap() == with_file,
        "a refused import changed the image"
    );
    assert_ne!(
        with_file, before,
        "the import before the refusals wrote nothing"
    );
    assert!(staged(&img, "/").is_empty(), "{:?}", staged(&img, "/"));
}

#[test]
fn an_import_that_runs_out_of_space_keeps_the_old_file_and_leaves_no_staged_name() {
    let img = volume("import-full");
    let dir = img.parent().unwrap().to_path_buf();
    let old = pattern(50_000, 3);
    let old_path = source(&dir, "old", &old);
    ok(fs_btrfs(&img).arg("import").arg(&old_path).arg("/old.bin"));

    // Twice what the volume's one 8 MiB data block group holds.
    let huge = source(&dir, "huge", &pattern(16 << 20, 4));
    let out = fs_btrfs(&img)
        .args(["import", "--replace"])
        .arg(&huge)
        .arg("/old.bin")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "an import larger than the volume succeeded: {}",
        stdout(&out)
    );
    assert!(
        stderr(&out).contains("free"),
        "the failure does not say the volume is full: {}",
        stderr(&out)
    );
    assert_eq!(
        read(&img, "/old.bin"),
        old,
        "the old file changed when its replacement ran out of space"
    );
    assert!(staged(&img, "/").is_empty(), "{:?}", staged(&img, "/"));
    // The space the staged copy took is given back: the same import of
    // the old bytes still fits.
    ok(fs_btrfs(&img)
        .args(["import", "--replace"])
        .arg(&old_path)
        .arg("/old.bin"));
    // And the volume still takes writes from the library.
    let mut fs = mount_rw(&img);
    let root = fs.lookup_path("/").unwrap().ino;
    let ino = fs.create(root, b"after", 0o644, 0, 0).unwrap();
    fs.write(ino, 0, b"still writable").unwrap();
}

#[test]
fn an_import_cancelled_by_a_signal_publishes_nothing() {
    let img = volume("import-signal");
    let mut child = fs_btrfs(&img)
        .args(["import", "-", "/cancelled.bin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(&pattern(1 << 20, 5)).unwrap();
    // The staged name appears once the import has begun.
    let deadline = Instant::now() + Duration::from_secs(60);
    while !staging(&img) {
        assert!(Instant::now() < deadline, "the import never staged a file");
        assert!(
            child.try_wait().unwrap().is_none(),
            "the import exited before it was signalled"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let sent = std::process::Command::new("kill")
        .args(["-s", "TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(sent.success(), "kill -s TERM");
    // The next chunk is where the import notices the signal.
    let _ = input.write_all(&pattern(1 << 20, 6));
    drop(input);
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success(), "a signalled import succeeded");
    assert!(
        stderr(&out).contains("cancelled"),
        "the failure does not say it was cancelled: {}",
        stderr(&out)
    );
    assert!(
        !exists(&img, "/cancelled.bin"),
        "a cancelled import published"
    );
    assert!(staged(&img, "/").is_empty(), "{:?}", staged(&img, "/"));
}

#[test]
fn a_tree_is_imported_file_by_file_and_merges_into_a_directory_there() {
    let img = volume("import-tree");
    let dir = img.parent().unwrap().to_path_buf();
    let tree = dir.join("tree");
    std::fs::create_dir_all(tree.join("a/b")).unwrap();
    let leaf = pattern(300_000, 7);
    std::fs::write(tree.join("a/b/leaf.bin"), &leaf).unwrap();
    std::fs::write(tree.join("top.txt"), b"top\n").unwrap();
    std::fs::create_dir(tree.join("empty")).unwrap();
    ok(fs_btrfs(&img).args(["mkdir", "/tree"]));

    let out = ok(fs_btrfs(&img)
        .args(["import", "--recursive"])
        .arg(&tree)
        .arg("/tree"));
    assert_eq!(json_field(&stdout(&out), "failed"), "0", "{}", stdout(&out));
    assert_eq!(read(&img, "/tree/a/b/leaf.bin"), leaf);
    assert_eq!(read(&img, "/tree/top.txt"), b"top\n");
    assert!(exists(&img, "/tree/empty"));
    for d in ["/tree", "/tree/a", "/tree/a/b"] {
        assert!(staged(&img, d).is_empty(), "{d}: {:?}", staged(&img, d));
    }
    // --verify reads one staged file back; a tree is refused, not unverified.
    refused(
        fs_btrfs(&img)
            .args(["import", "--recursive", "--verify"])
            .arg(&tree)
            .arg("/again"),
        2,
    );
}
