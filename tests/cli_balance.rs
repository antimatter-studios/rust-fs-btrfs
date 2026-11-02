//! `fs.btrfs balance` (#402): the filters as `btrfs balance start` spells
//! them, reached from the command line on this crate's own `mkfs.btrfs`
//! volume.
//!
//! Each case is judged by the block groups the library then reads, and each
//! refusal leaves the image byte-identical. What the kernel and `btrfs
//! check` make of a balanced volume is tests/cli_balance_kernel.rs.
//!
//! No fixture, no VM: the unit tier.

mod cli_support;

use cli_support::*;
use fs_btrfs::chunk::block_group as bg;
use fs_btrfs::{ChunkProfile, Filesystem};
use fs_core::{BlockDevice, FileDevice};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn payload() -> Vec<u8> {
    (0..200_000u32).map(|i| (i % 241) as u8).collect()
}

fn mount_rw(img: &Path) -> Filesystem {
    let dev = Arc::new(FileDevice::open_rw(img).unwrap());
    Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).unwrap()
}

/// A fresh 256 MiB volume made by this crate's `mkfs.btrfs`, holding one
/// file.
fn volume(tag: &str) -> PathBuf {
    let img = scratch_dir(tag).join("balance.img");
    ok(tool("mkfs.btrfs")
        .args(["-q", "--text", "--size", "256M"])
        .arg(&img));
    let mut fs = mount_rw(&img);
    let root = fs.lookup_path("/").unwrap().ino;
    let file = fs.create(root, b"file", 0o644, 0, 0).unwrap();
    fs.write(file, 0, &payload()).unwrap();
    img
}

/// `fs.btrfs IMAGE balance ARGS...` required to succeed: the report.
fn balance_ok(img: &Path, args: &[&str]) -> String {
    stdout(&ok(tool("fs.btrfs").arg(img).arg("balance").args(args)))
}

/// How many groups a report says moved.
fn moved(report: &str) -> usize {
    report.matches("\"from\"").count()
}

/// Every group holding `kind`: where it starts, and its profile.
fn groups(img: &Path, kind: u64) -> Vec<(u64, ChunkProfile)> {
    mount_rw(img)
        .block_groups()
        .unwrap()
        .iter()
        .filter(|g| g.flags & kind != 0)
        .map(|g| (g.start, ChunkProfile::from_type(g.flags).unwrap()))
        .collect()
}

fn file_bytes(img: &Path) -> Vec<u8> {
    let fs = mount_rw(img);
    let ino = fs.lookup_path("/file").unwrap().ino;
    fs.read_file(ino).unwrap()
}

#[test]
fn help_describes_the_verb() {
    let help = stdout(&ok(tool("fs.btrfs").args([
        "image.img",
        "balance",
        "--help",
    ])));
    for word in ["-d", "-m", "convert=", "usage=", "profiles=", "soft"] {
        assert!(help.contains(word), "balance --help omits {word}: {help}");
    }
}

#[test]
fn dash_d_moves_data_groups_and_leaves_the_metadata_groups() {
    let img = volume("balance-data");
    let (data, meta) = (groups(&img, bg::DATA), groups(&img, bg::METADATA));
    let report = balance_ok(&img, &["-d"]);
    assert_eq!(moved(&report), data.len(), "{report}");
    let after = groups(&img, bg::DATA);
    assert!(
        after.iter().all(|g| !data.contains(g)),
        "a data group stayed: {data:?} then {after:?}"
    );
    assert_eq!(groups(&img, bg::METADATA), meta);
    assert_eq!(file_bytes(&img), payload());
}

#[test]
fn dash_m_moves_metadata_groups_and_leaves_the_data_groups() {
    let img = volume("balance-metadata");
    let (data, meta) = (groups(&img, bg::DATA), groups(&img, bg::METADATA));
    let report = balance_ok(&img, &["--metadata"]);
    assert_eq!(moved(&report), meta.len(), "{report}");
    let after = groups(&img, bg::METADATA);
    assert!(
        after.iter().all(|g| !meta.contains(g)),
        "a metadata group stayed: {meta:?} then {after:?}"
    );
    assert_eq!(groups(&img, bg::DATA), data);
    assert_eq!(file_bytes(&img), payload());
}

#[test]
fn convert_gives_the_new_chunks_the_profile_and_soft_leaves_what_has_it() {
    let img = volume("balance-convert");
    let report = balance_ok(&img, &["-dconvert=dup"]);
    assert!(moved(&report) >= 1, "{report}");
    assert!(groups(&img, bg::DATA)
        .iter()
        .all(|(_, p)| *p == ChunkProfile::Dup));
    assert_eq!(file_bytes(&img), payload());

    let was = groups(&img, bg::DATA);
    for args in ["-dconvert=dup", "-dconvert=dup,soft"] {
        let report = balance_ok(&img, &[args]);
        assert_eq!(moved(&report), 0, "{args}: {report}");
        assert_eq!(groups(&img, bg::DATA), was, "{args}");
    }

    let report = balance_ok(&img, &["-dconvert=single"]);
    assert_eq!(moved(&report), was.len(), "{report}");
    assert!(groups(&img, bg::DATA)
        .iter()
        .all(|(_, p)| *p == ChunkProfile::Single));
    assert_eq!(file_bytes(&img), payload());
}

#[test]
fn profiles_and_usage_narrow_what_is_selected() {
    let img = volume("balance-filters");
    let was = groups(&img, bg::DATA);
    // Single, so a filter naming only others selects nothing; and the file
    // is in it, so a usage of zero, which takes only empty groups, does not.
    for args in ["-dprofiles=dup", "-dprofiles=raid1|raid10", "-dusage=0"] {
        let report = balance_ok(&img, &[args]);
        assert_eq!(moved(&report), 0, "{args}: {report}");
        assert_eq!(groups(&img, bg::DATA), was, "{args}");
    }
    let report = balance_ok(&img, &["-dprofiles=single|dup,usage=100"]);
    assert_eq!(moved(&report), was.len(), "{report}");
    assert_eq!(file_bytes(&img), payload());
}

#[test]
fn a_refused_balance_leaves_the_image_as_it_was() {
    let img = volume("balance-refused");
    let before = std::fs::read(&img).unwrap();
    for args in [
        // One device holds single and DUP, nothing else.
        &["-dconvert=raid1"][..],
        // The data groups alone could move: the check comes before any does.
        &["-d", "-mconvert=raid1"][..],
    ] {
        let message = refused(tool("fs.btrfs").arg(&img).arg("balance").args(args), 3);
        assert!(
            std::fs::read(&img).unwrap() == before,
            "a refused `balance {}` changed the image: {message}",
            args.join(" ")
        );
    }
}
