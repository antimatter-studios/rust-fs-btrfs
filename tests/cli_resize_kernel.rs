//! `fs.btrfs resize` grows and shrinks a filesystem and the image file it
//! lives in, judged by what is not this crate (#264).
//!
//! The library's resize is checked through the C ABI in
//! `resize_kernel.rs`. What the tool adds is the image file: made longer
//! before a grow, cut after a shrink, and given its length back when the
//! resize is refused after it was made longer. So every case here looks at
//! the file as well as the filesystem: `dump-super` reads the two sizes
//! back, `btrfs check` inspects the whole volume, and the kernel mounts
//! it, reports the size it believes and, for a grown one, fills it past
//! the old end, which it can only do by allocating in the new space.

mod cli_support;

use cli_support::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_super, guest_kernel_read_ok, guest_kernel_write_ok, oracle,
};
use std::path::{Path, PathBuf};

const MIB: u64 = 1 << 20;

/// A scratch volume of `size` bytes made by the reference `mkfs.btrfs`,
/// removed when it drops, including on a panic.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str, size: u64) -> Self {
        let scratch = Scratch(scratch_dir(tag));
        let image = scratch.image();
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(size))
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
        scratch
    }

    /// The same, with a file the kernel wrote, so the volume holds data.
    fn with_data(tag: &str, size: u64) -> Self {
        let scratch = Scratch::new(tag, size);
        guest_kernel_write_ok(
            &scratch.image().to_string_lossy(),
            tag,
            "head -c 100000 /dev/urandom > \"$MNT/before.bin\"\nsync",
        );
        scratch
    }

    fn image(&self) -> PathBuf {
        self.0.join("fs.img")
    }

    fn len(&self) -> u64 {
        std::fs::metadata(self.image()).unwrap().len()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `fs.btrfs IMAGE resize ARGS...`, required to succeed; its JSON report.
fn resize_ok(image: &Path, args: &[&str]) -> String {
    stdout(&ok(tool("fs.btrfs").arg(image).arg("resize").args(args)))
}

/// `fs.btrfs IMAGE resize ARGS...`, required to be refused with exit 3
/// and to leave the image exactly as it was, length included.
fn resize_refused(image: &Path, args: &[&str]) -> String {
    let before = std::fs::read(image).unwrap();
    let message = refused(tool("fs.btrfs").arg(image).arg("resize").args(args), 3);
    assert!(
        std::fs::read(image).unwrap() == before,
        "a refused `resize {}` changed the image: {message}",
        args.join(" ")
    );
    message
}

/// The superblock's size and its device item's, from `dump-super`.
fn sizes(image: &Path) -> (u64, u64) {
    let text = dump_super(image);
    let field = |name: &str| {
        text.lines()
            .find_map(|l| {
                let mut words = l.split_whitespace();
                (words.next() == Some(name)).then(|| words.next().unwrap_or("").to_string())
            })
            .unwrap_or_else(|| panic!("dump-super has no {name}:\n{text}"))
            .parse::<u64>()
            .unwrap_or_else(|e| panic!("dump-super's {name} is not a number ({e}):\n{text}"))
    };
    (field("total_bytes"), field("dev_item.total_bytes"))
}

/// What the kernel says the device's size is.
fn kernel_size(image: &Path, what: &str) -> u64 {
    let said = guest_kernel_read_ok(
        &image.to_string_lossy(),
        what,
        "btrfs filesystem usage -b \"$MNT\" | awk '/Device size:/ {print $3}'",
    );
    said.trim()
        .parse()
        .unwrap_or_else(|e| panic!("the kernel's device size {said:?} is not a number: {e}"))
}

#[test]
fn a_grown_image_is_longer_clean_and_the_kernel_fills_it() {
    let scratch = Scratch::with_data("resize-grow", 256 * MIB);
    let image = scratch.image();

    // +256M from 256 MiB: the tool makes the file longer, then the
    // filesystem.
    let report = resize_ok(&image, &["+256M"]);
    assert_eq!(
        json_field(&report, "previous_total_bytes"),
        (256 * MIB).to_string()
    );
    assert_eq!(json_field(&report, "total_bytes"), (512 * MIB).to_string());
    assert_eq!(json_field(&report, "device_bytes"), (512 * MIB).to_string());
    assert_eq!(
        scratch.len(),
        512 * MIB,
        "the image file was not made longer"
    );

    assert_btrfs_check_clean(&image, "after fs.btrfs resize +256M");
    assert_eq!(sizes(&image), (512 * MIB, 512 * MIB));
    assert_eq!(kernel_size(&image, "after growing"), 512 * MIB);

    // 300 MiB of data does not fit in the old 256 MiB: the kernel must
    // allocate chunks in the space the resize added.
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "the kernel fills the grown volume",
        "dd if=/dev/zero of=\"$MNT/big\" bs=1M count=300 status=none\nsync",
    );
    assert_btrfs_check_clean(&image, "after the kernel filled the grown volume");

    // A file made longer by hand, then `max`: the filesystem takes all of
    // it, and the file keeps its length.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&image)
        .and_then(|f| f.set_len(640 * MIB))
        .unwrap();
    let report = resize_ok(&image, &["max"]);
    assert_eq!(json_field(&report, "total_bytes"), (640 * MIB).to_string());
    assert_eq!(scratch.len(), 640 * MIB);
    assert_btrfs_check_clean(&image, "after fs.btrfs resize max");
    assert_eq!(sizes(&image), (640 * MIB, 640 * MIB));
}

#[test]
fn a_shrink_needs_force_stops_at_the_last_chunk_and_cuts_the_image() {
    let scratch = Scratch::with_data("resize-shrink", 512 * MIB);
    let image = scratch.image();

    // Without --force: refused before anything is written.
    let message = resize_refused(&image, &["448M"]);
    assert!(message.contains("--force"), "{message}");

    // Past the chunks mkfs placed: moving them is a balance.
    let message = resize_refused(&image, &["32M", "--force"]);
    assert!(message.contains("balance"), "{message}");

    let report = resize_ok(&image, &["-64M", "--force"]);
    assert_eq!(
        json_field(&report, "previous_total_bytes"),
        (512 * MIB).to_string()
    );
    assert_eq!(json_field(&report, "total_bytes"), (448 * MIB).to_string());
    assert_eq!(json_field(&report, "device_bytes"), (448 * MIB).to_string());
    assert_eq!(scratch.len(), 448 * MIB, "the image file was not cut");

    assert_btrfs_check_clean(&image, "after fs.btrfs resize -64M --force");
    assert_eq!(sizes(&image), (448 * MIB, 448 * MIB));
    assert_eq!(kernel_size(&image, "after shrinking"), 448 * MIB);
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "the kernel writes to the shrunk volume",
        "head -c 1000000 /dev/urandom > \"$MNT/after.bin\"\nsync",
    );
    assert_btrfs_check_clean(&image, "after the kernel wrote to the shrunk volume");
}

#[test]
fn a_grow_the_library_refuses_gives_the_image_its_length_back() {
    // The reference tool makes the volume a seed, which is read-only by
    // construction: the tool has to make the file longer before the
    // library can say so.
    let scratch = Scratch::new("resize-seed", 256 * MIB);
    let image = scratch.image();
    let tuned = oracle("btrfstune").args(["-S", "1"]).arg(&image).output();
    assert!(
        tuned.status.success(),
        "btrfstune -S 1: {}",
        String::from_utf8_lossy(&tuned.stderr)
    );

    let message = resize_refused(&image, &["512M"]);
    assert!(message.contains("seed"), "{message}");
    assert_eq!(
        scratch.len(),
        256 * MIB,
        "the refused grow left the file longer"
    );
    assert_btrfs_check_clean(&image, "after a refused fs.btrfs resize");
}
