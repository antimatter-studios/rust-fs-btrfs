//! `fs.btrfs balance` relocates and converts the block groups of a
//! kernel-written volume, judged by what is not this crate (#402).
//!
//! The library's relocation is checked group by group in the
//! `relocate_*_kernel.rs` targets. What the tool adds is the selection:
//! `-d` and `-m` with the filters `btrfs balance start` spells, every
//! group selected relocated, and a refusal leaving the image as it was.
//!
//! # The oracles
//!
//! The kernel writes the files and reports every one of them, with its
//! SHA-256, before the tool touches the volume. After each balance,
//! `btrfs check --readonly` inspects the whole volume, `btrfs filesystem
//! df` on a kernel mount names the profiles the chunks now have, and the
//! kernel's report must be the one it gave before. Last, the kernel
//! writes into the balanced volume, which allocates beside the chunks
//! this crate made, and `btrfs check` looks again.

mod cli_support;

use cli_support::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, guest_kernel_read_ok, guest_kernel_report, guest_kernel_write_ok,
    oracle,
};
use std::path::{Path, PathBuf};

/// The kernel's files: a few sizes, a reflink copy, one partly
/// overwritten afterwards, a preallocated one and a `nodatasum` one.
const FILL: &str = "cd \"$MNT\"\n\
    head -c 4096 /dev/urandom > small.bin\n\
    head -c 300000 /dev/urandom > medium.bin\n\
    head -c 3000000 /dev/urandom > large.bin\n\
    cp --reflink=always large.bin large.reflink.bin\n\
    dd if=/dev/urandom of=large.bin bs=4096 seek=100 count=10 conv=notrunc status=none\n\
    fallocate -l 1048576 prealloc.bin\n\
    mkdir nc\n\
    chattr +C nc\n\
    head -c 200000 /dev/urandom > nc/nodatasum.bin\n\
    mkdir dir\n\
    for i in 1 2 3 4 5 6 7 8; do head -c $((i * 20000)) /dev/urandom > dir/f$i; done\n\
    sync";

/// A scratch volume the reference `mkfs.btrfs` made, single data and DUP
/// metadata, holding [`FILL`]; removed when it drops, including on a
/// panic.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let scratch = Scratch(scratch_dir(tag));
        let image = scratch.image();
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(512 << 20))
            .unwrap();
        let made = oracle("mkfs.btrfs")
            .args([
                "-q", "-f", "-d", "single", "-m", "dup", "-s", "4096", "-n", "16384",
            ])
            .arg(&image)
            .output();
        assert!(
            made.status.success(),
            "mkfs.btrfs: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        guest_kernel_write_ok(&image.to_string_lossy(), tag, FILL);
        scratch
    }

    fn image(&self) -> PathBuf {
        self.0.join("fs.img")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `fs.btrfs IMAGE balance ARGS...`, required to succeed; each group it
/// relocated, as `(from, to)`.
fn balance_ok(image: &Path, args: &[&str]) -> Vec<(u64, u64)> {
    let report = stdout(&ok(tool("fs.btrfs").arg(image).arg("balance").args(args)));
    // One `"key": N` a line, as the report is printed.
    let values = |key: &str| -> Vec<u64> {
        let head = format!("\"{key}\": ");
        report
            .lines()
            .filter_map(|l| l.trim().strip_prefix(head.as_str()))
            .map(|v| {
                v.trim_end_matches(',')
                    .parse()
                    .unwrap_or_else(|e| panic!("{v:?} is not a number ({e}) in:\n{report}"))
            })
            .collect()
    };
    let (from, to) = (values("from"), values("to"));
    assert_eq!(from.len(), to.len(), "{report}");
    assert!(
        report.contains("\"relocated\": "),
        "no relocated list:\n{report}"
    );
    from.into_iter().zip(to).collect()
}

/// `btrfs filesystem df`'s profile lines, as the kernel reports them on a
/// mount: `Data, single`, `Metadata, DUP` and so on.
fn profiles(image: &Path, what: &str) -> Vec<String> {
    guest_kernel_read_ok(
        &image.to_string_lossy(),
        what,
        "btrfs filesystem df \"$MNT\"",
    )
    .lines()
    .filter_map(|l| l.split_once(':').map(|(head, _)| head.trim().to_string()))
    .collect()
}

#[test]
fn every_data_group_is_relocated_and_every_file_reads_back() {
    let scratch = Scratch::new("balance-data");
    let image = scratch.image();
    let before = guest_kernel_report(&image.to_string_lossy(), "before the balance");

    let moved = balance_ok(&image, &["-d"]);
    assert!(!moved.is_empty(), "balance -d relocated no group");
    for &(from, to) in &moved {
        assert_ne!(from, to, "a group was relocated onto itself");
    }

    assert_btrfs_check_clean(&image, "after fs.btrfs balance -d");
    assert_eq!(
        guest_kernel_report(&image.to_string_lossy(), "after the balance"),
        before,
        "the kernel reads the files differently after the balance"
    );
    let df = profiles(&image, "the profiles after balance -d");
    assert!(df.iter().any(|p| p == "Data, single"), "{df:?}");

    // A second time, the groups the first made are moved again.
    let again = balance_ok(&image, &["--data"]);
    assert_eq!(again.len(), moved.len(), "{again:?} after {moved:?}");
    for (&(_, to), &(from, _)) in moved.iter().zip(&again) {
        assert_eq!(
            from, to,
            "the second balance did not start from the first's groups"
        );
    }
    assert_btrfs_check_clean(&image, "after a second fs.btrfs balance -d");

    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "the kernel writes into the balanced volume",
        "head -c 5000000 /dev/urandom > \"$MNT/after.bin\"\nsync",
    );
    assert_btrfs_check_clean(&image, "after the kernel wrote to the balanced volume");
}

#[test]
fn data_converts_to_dup_and_metadata_to_single_in_one_balance() {
    let scratch = Scratch::new("balance-convert");
    let image = scratch.image();
    let before = guest_kernel_report(&image.to_string_lossy(), "before the conversion");
    let df = profiles(&image, "the profiles mkfs.btrfs made");
    assert!(
        df.iter().any(|p| p == "Data, single") && df.iter().any(|p| p == "Metadata, DUP"),
        "the volume does not start single data and DUP metadata: {df:?}"
    );

    let moved = balance_ok(&image, &["-dconvert=dup", "-mconvert=single"]);
    assert!(moved.len() >= 2, "{moved:?}");

    assert_btrfs_check_clean(&image, "after balance -dconvert=dup -mconvert=single");
    let df = profiles(&image, "the profiles after the conversion");
    assert!(df.iter().any(|p| p == "Data, DUP"), "{df:?}");
    assert!(df.iter().any(|p| p == "Metadata, single"), "{df:?}");
    assert!(!df.iter().any(|p| p == "Data, single"), "{df:?}");
    assert!(!df.iter().any(|p| p == "Metadata, DUP"), "{df:?}");
    assert_eq!(
        guest_kernel_report(&image.to_string_lossy(), "after the conversion"),
        before,
        "the kernel reads the files differently after the conversion"
    );

    // Converted already: soft leaves every group where it is.
    assert!(balance_ok(&image, &["-dconvert=dup,soft"]).is_empty());

    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "the kernel writes into the converted volume",
        "head -c 5000000 /dev/urandom > \"$MNT/after.bin\"\nsync",
    );
    assert_btrfs_check_clean(&image, "after the kernel wrote to the converted volume");
    let df = profiles(&image, "the profiles after the kernel wrote");
    assert!(!df.iter().any(|p| p == "Data, single"), "{df:?}");
}

#[test]
fn a_balance_the_library_refuses_leaves_the_image_as_it_was() {
    let scratch = Scratch::new("balance-refused");
    let image = scratch.image();
    let before = std::fs::read(&image).unwrap();
    for args in [
        // One device holds single and DUP, nothing else.
        &["-dconvert=raid1"][..],
        // The metadata groups are DUP, and a DUP chunk is only allocated
        // to convert a single one.
        &["-d", "-m"][..],
    ] {
        let message = refused(tool("fs.btrfs").arg(&image).arg("balance").args(args), 3);
        assert!(
            std::fs::read(&image).unwrap() == before,
            "a refused `balance {}` changed the image: {message}",
            args.join(" ")
        );
    }
}
