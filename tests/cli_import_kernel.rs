//! `fs.btrfs import` (#431), judged by what is not this crate: the Linux
//! kernel mounts each image the tool imported into, digests the files with
//! `sha256sum` and lists the names it finds, and `btrfs check --readonly`
//! grades the volume. Both run in the harness VM. The digests the kernel
//! reports are compared with the SHA-256 of the local source, computed
//! here, not with what the tool reported.
//!
//! - a 64 MiB file under `--verify` and a one-byte file read back by the
//!   kernel with the source's SHA-256, and a tree imported with
//!   `--recursive` read back file by file under the names it was given;
//! - an import that runs out of space on a nearly full volume, replacing
//!   a file, leaves the old file byte-identical when the kernel reads it,
//!   no staged name behind, and the volume clean;
//! - an import killed (SIGKILL) between persist and publish, replacing a
//!   file, leaves the old file under that name, and the volume clean. A
//!   staged name left by the kill is allowed, and is reported.
//!
//! The volumes are made by `mkfs.btrfs` from btrfs-progs. This library
//! does not allocate block groups, and a fresh volume has one 8 MiB data
//! group, so the large imports go to a volume the kernel has grown: it
//! writes a pad file, which allocates data groups, and `fs.btrfs rm`
//! releases it, leaving the groups free. The kernel would delete a group
//! its own `rm` emptied.

mod cli_support;

use cli_support::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, guest_kernel_read_ok, guest_kernel_write_ok, oracle, sha256_hex,
};
use std::path::{Path, PathBuf};
use std::process::Stdio;
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

/// A `size_mib` volume from `mkfs.btrfs`. With `pad_mib`, the kernel
/// writes that much into a file, allocating data groups for it, and
/// `fs.btrfs rm` releases the file, so the groups stay and are free.
fn volume(dir: &Path, size_mib: u64, pad_mib: u64) -> PathBuf {
    let image = dir.join("import.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(size_mib << 20))
        .unwrap();
    let made = oracle("mkfs.btrfs")
        .args(["-q", "-f", "-s", "4096", "-n", "16384"])
        .arg(&image)
        .output();
    assert!(
        made.status.success(),
        "mkfs.btrfs: {}",
        String::from_utf8_lossy(&made.stderr)
    );
    if pad_mib > 0 {
        guest_kernel_write_ok(
            image.to_str().unwrap(),
            "the kernel grows the volume's data groups",
            &format!("head -c {pad_mib}M /dev/zero > \"$MNT/pad\"\nsync"),
        );
        ok(tool("fs.btrfs").arg(&image).args(["rm", "/pad"]));
    }
    image
}

fn source(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// What the kernel made of `image`: the SHA-256 it reads for each of
/// `files`, and every name under `/`, as paths, sorted.
struct Graded {
    digests: Vec<(String, String)>,
    names: Vec<String>,
}

impl Graded {
    fn sum(&self, path: &str) -> &str {
        &self
            .digests
            .iter()
            .find(|(p, _)| p == path)
            .unwrap_or_else(|| panic!("the kernel digested no {path}"))
            .1
    }

    fn staged(&self) -> Vec<&String> {
        self.names
            .iter()
            .filter(|n| n.rsplit('/').next().unwrap().starts_with(STAGED))
            .collect()
    }
}

/// `btrfs check` on `image`, then one kernel mount reading it back.
fn grade(image: &Path, what: &str, files: &[&str]) -> Graded {
    assert_btrfs_check_clean(image, what);
    let mut script = String::new();
    for f in files {
        script.push_str(&format!(
            "echo \"SHA $(sha256sum \"$MNT{f}\" | cut -d' ' -f1) {f}\"\n"
        ));
    }
    script.push_str("(cd \"$MNT\" && find . -mindepth 1 | sort | sed 's/^\\./NAME /')\n");
    let out = guest_kernel_read_ok(image.to_str().unwrap(), what, &script);
    Graded {
        digests: out
            .lines()
            .filter_map(|l| l.strip_prefix("SHA "))
            .filter_map(|l| l.split_once(' '))
            .map(|(sum, path)| (path.to_string(), sum.to_string()))
            .collect(),
        names: out
            .lines()
            .filter_map(|l| l.strip_prefix("NAME "))
            .map(String::from)
            .collect(),
    }
}

#[test]
fn imports_read_back_through_the_kernel_with_the_digest_sent() {
    let dir = scratch_dir("import-kernel");
    let image = volume(&dir, 1024, 400);

    let big = pattern(64 << 20, 11);
    let big_path = source(&dir, "big", &big);
    let out = ok(tool("fs.btrfs")
        .arg(&image)
        .args(["import", "--verify"])
        .arg(&big_path)
        .arg("/big.bin"));
    assert_eq!(json_field(&stdout(&out), "sha256"), sha256_hex(&big));
    assert_eq!(json_field(&stdout(&out), "verified"), "true");
    let one_path = source(&dir, "one", b"z");
    ok(tool("fs.btrfs")
        .arg(&image)
        .arg("import")
        .arg(&one_path)
        .arg("/one.bin"));

    let tree = dir.join("tree");
    std::fs::create_dir_all(tree.join("a/b")).unwrap();
    let leaf = pattern(300_000, 12);
    std::fs::write(tree.join("a/b/leaf.bin"), &leaf).unwrap();
    std::fs::write(tree.join("top.txt"), b"top\n").unwrap();
    let out = ok(tool("fs.btrfs")
        .arg(&image)
        .args(["import", "--recursive"])
        .arg(&tree)
        .arg("/tree"));
    assert_eq!(json_field(&stdout(&out), "failed"), "0", "{}", stdout(&out));

    let graded = grade(
        &image,
        "after fs.btrfs import",
        &[
            "/big.bin",
            "/one.bin",
            "/tree/a/b/leaf.bin",
            "/tree/top.txt",
        ],
    );
    assert_eq!(graded.sum("/big.bin"), sha256_hex(&big), "64 MiB file");
    assert_eq!(graded.sum("/one.bin"), sha256_hex(b"z"), "one-byte file");
    assert_eq!(graded.sum("/tree/a/b/leaf.bin"), sha256_hex(&leaf));
    assert_eq!(graded.sum("/tree/top.txt"), sha256_hex(b"top\n"));
    assert_eq!(
        graded.names,
        [
            "/big.bin",
            "/one.bin",
            "/tree",
            "/tree/a",
            "/tree/a/b",
            "/tree/a/b/leaf.bin",
            "/tree/top.txt",
        ],
        "the kernel finds other names than were imported"
    );
}

#[test]
fn an_import_that_runs_out_of_space_leaves_the_old_file_and_a_clean_volume() {
    let dir = scratch_dir("import-full-kernel");
    let image = volume(&dir, 256, 0);
    let old = pattern(50_000, 21);
    let old_path = source(&dir, "old", &old);
    ok(tool("fs.btrfs")
        .arg(&image)
        .arg("import")
        .arg(&old_path)
        .arg("/old.bin"));

    // 300 MiB of zeros, as a hole: more than the 256 MiB volume holds.
    let huge = dir.join("huge");
    std::fs::File::create(&huge)
        .and_then(|f| f.set_len(300 << 20))
        .unwrap();
    let out = tool("fs.btrfs")
        .arg(&image)
        .args(["import", "--replace"])
        .arg(&huge)
        .arg("/old.bin")
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&huge);
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

    let graded = grade(&image, "after an import ran out of space", &["/old.bin"]);
    assert_eq!(
        graded.sum("/old.bin"),
        sha256_hex(&old),
        "the old file changed when its replacement ran out of space"
    );
    assert_eq!(graded.names, ["/old.bin"], "names left behind");
}

/// Whether `image` holds a staged file of exactly `len` bytes: every
/// write committed and persist done, publish not yet. Read through a fresh
/// read-only open, which never changes the image; a read that meets a
/// generation being committed is "not yet".
fn persisted_not_published(image: &Path, len: u64) -> bool {
    let Ok(listing) = tool("fs.btrfs").arg(image).args(["ls", "/"]).output() else {
        return false;
    };
    let listing = stdout(&listing);
    listing.split('{').any(|entry| {
        entry.contains(STAGED) && json_field(&format!("{{{entry}"), "size") == len.to_string()
    })
}

#[test]
fn an_import_killed_between_persist_and_publish_leaves_the_old_file_and_a_clean_volume() {
    let dir = scratch_dir("import-kill-kernel");
    let base = volume(&dir, 1024, 400);
    let old = pattern(70_000, 31);
    let old_path = source(&dir, "old", &old);
    ok(tool("fs.btrfs")
        .arg(&base)
        .arg("import")
        .arg(&old_path)
        .arg("/victim.bin"));
    let new = pattern(160 << 20, 32);
    let new_path = source(&dir, "new", &new);

    // The window between persist and publish is the read-back of the
    // whole staged file under --verify. It is found by watching the
    // image, not by guessing a delay, and tried again when a run was too
    // quick to catch.
    for attempt in 1..=5 {
        let image = dir.join(format!("kill-{attempt}.img"));
        std::fs::copy(&base, &image).unwrap();
        let mut child = tool("fs.btrfs")
            .arg(&image)
            .args(["import", "--replace", "--verify"])
            .arg(&new_path)
            .arg("/victim.bin")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(300);
        let mut caught = false;
        while Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if persisted_not_published(&image, new.len() as u64) {
                caught = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = child.kill();
        let status = child.wait().unwrap();
        if !caught || status.code().is_some() {
            eprintln!("attempt {attempt}: the import was past the window; trying again");
            continue;
        }

        let graded = grade(
            &image,
            "after an import killed between persist and publish",
            &["/victim.bin"],
        );
        assert_eq!(
            graded.sum("/victim.bin"),
            sha256_hex(&old),
            "an import killed between persist and publish changed the old file"
        );
        for name in graded.staged() {
            eprintln!("killed between persist and publish: staged name left, reported: {name}");
        }
        assert!(
            graded
                .names
                .iter()
                .all(|n| n == "/victim.bin" || graded.staged().contains(&n)),
            "names other than the file and its staged copy: {:?}",
            graded.names
        );
        return;
    }
    panic!("no attempt caught the import between persist and publish");
}
