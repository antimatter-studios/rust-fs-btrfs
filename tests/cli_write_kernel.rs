//! `fs.btrfs write`, judged by what is not this crate. After each write
//! this library can make -- a NODATACOW file overwritten in place, and an
//! ordinary copy-on-write file rewritten into new extents (#274) --
//! `btrfs check --readonly` finds the volume clean, and the Linux kernel
//! mounts it and reads back exactly the bytes `fs.btrfs` was given, with
//! the files around it as the kernel first wrote them. Both judgements run
//! in the harness VM.

mod cli_support;

use cli_support::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, fixture, guest_kernel_read_ok, guest_kernel_write_ok, oracle,
    sha256_hex,
};
use std::io::Write;
use std::process::Stdio;

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

/// The kernel's manifest line for `path`: (size, sha256).
fn manifest_file(path: &str) -> (usize, String) {
    let manifest = std::fs::read_to_string(fixture("cli/btrfs-cli.manifest")).unwrap();
    manifest
        .lines()
        .map(|l| l.split('\t').collect::<Vec<_>>())
        .find(|f| f.len() == 5 && f[0] == "f" && f[1] == path)
        .map(|f| (f[2].parse().unwrap(), f[3].to_string()))
        .unwrap_or_else(|| panic!("the manifest has no file {path}:\n{manifest}"))
}

#[test]
fn a_nodatacow_overwrite_is_clean_to_btrfs_check_and_read_back_by_the_kernel() {
    let image = scratch_dir("write-kernel").join("btrfs-cli.img");
    std::fs::copy(fixture("cli/btrfs-cli.img"), &image).expect("copy the fixture");
    let (size, _) = manifest_file("/nocow/data.bin");
    let bytes = pattern(size, 225);

    let mut child = tool("fs.btrfs")
        .arg(&image)
        .args(["write", "/nocow/data.bin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fs.btrfs write");
    child.stdin.take().unwrap().write_all(&bytes).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "fs.btrfs write: {}", stderr(&out));
    assert_eq!(json_field(&stdout(&out), "bytes"), size.to_string());

    assert_btrfs_check_clean(&image, "after fs.btrfs write");
    // One mount: the written file, and the files around it, which must
    // still be the kernel's own.
    let image = image.to_str().expect("a UTF-8 scratch path").to_string();
    let others = [
        "/dir/random.bin",
        "/hello.txt",
        "/zstd/text.txt",
        "/snap/inside.txt",
    ];
    let mut script = String::new();
    for path in std::iter::once("/nocow/data.bin").chain(others) {
        script.push_str(&format!(r#"sha256sum "$MNT{path}" | cut -d' ' -f1"#));
        script.push('\n');
    }
    let got = guest_kernel_read_ok(&image, "after fs.btrfs write", &script);
    let got: Vec<&str> = got.lines().collect();
    assert_eq!(got.len(), 1 + others.len(), "{got:?}");
    assert_eq!(
        got[0],
        sha256_hex(&bytes),
        "the kernel reads back different bytes than fs.btrfs wrote"
    );
    for (path, got) in others.iter().zip(&got[1..]) {
        let (_, want) = manifest_file(path);
        assert_eq!(*got, want, "{path} changed under fs.btrfs write");
    }
}

/// A fresh 256 MiB volume from `mkfs.btrfs`, filled by the kernel: an
/// ordinary copy-on-write file in two extents (64 KiB, then 40,000 bytes
/// appended in a later transaction), written on a `nodatasum` mount
/// because a checksummed file is a case the library does not write yet
/// (#261); and a checksummed file beside it written before the remount.
fn kernel_made_cow_volume(tag: &str) -> std::path::PathBuf {
    let image = scratch_dir(tag).join("fs.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(256 << 20))
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
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        tag,
        "head -c 65536 /dev/urandom > \"$MNT/summed.bin\"\n\
         sync\n\
         mount -o remount,nodatasum \"$MNT\"\n\
         head -c 65536 /dev/urandom > \"$MNT/cow.bin\"\n\
         sync\n\
         head -c 40000 /dev/urandom >> \"$MNT/cow.bin\"\n\
         sync",
    );
    image
}

/// The kernel's view of `names`: each file's SHA-256 and the physical
/// address of its first extent, from `filefrag`.
fn kernel_view(image: &str, what: &str, names: &[&str]) -> Vec<(String, String)> {
    let mut script = String::new();
    for name in names {
        script.push_str(&format!(
            "printf '%s %s\\n' \"$(sha256sum \"$MNT/{name}\" | cut -d' ' -f1)\" \
             \"$(filefrag -v \"$MNT/{name}\" | awk '$1 == \"0:\" {{print $4; exit}}')\"\n"
        ));
    }
    guest_kernel_read_ok(image, what, &script)
        .lines()
        .map(|line| {
            let mut words = line.split_whitespace();
            (
                words.next().unwrap_or_default().to_string(),
                words.next().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// An ordinary copy-on-write file, overwritten whole through the CLI,
/// lands in new extents: `btrfs check` finds the volume clean, the kernel
/// reads back the bytes `fs.btrfs` was given from somewhere other than
/// where the file was, and the checksummed file beside it is unchanged.
#[test]
fn an_ordinary_copy_on_write_file_overwritten_by_the_cli_is_clean_and_read_back() {
    let image = kernel_made_cow_volume("write-cow-kernel");
    let path = image.to_str().expect("a UTF-8 scratch path").to_string();
    let before = kernel_view(&path, "before fs.btrfs write", &["cow.bin", "summed.bin"]);
    let bytes = pattern(105_536, 274);

    let mut child = tool("fs.btrfs")
        .arg(&image)
        .args(["write", "/cow.bin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fs.btrfs write");
    child.stdin.take().unwrap().write_all(&bytes).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "fs.btrfs write: {}", stderr(&out));
    assert_eq!(json_field(&stdout(&out), "bytes"), bytes.len().to_string());

    assert_btrfs_check_clean(&image, "after fs.btrfs write of a copy-on-write file");
    let after = kernel_view(&path, "after fs.btrfs write", &["cow.bin", "summed.bin"]);
    assert_eq!(
        after[0].0,
        sha256_hex(&bytes),
        "the kernel reads back different bytes than fs.btrfs wrote"
    );
    assert_ne!(
        after[0].1, before[0].1,
        "cow.bin's first extent is where it was, so the write was not copy-on-write"
    );
    assert_eq!(
        after[1], before[1],
        "summed.bin changed under fs.btrfs write"
    );
}
