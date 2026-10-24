//! Names linked through the C ABI past a full `INODE_REF`, judged by
//! what is not this crate (#425).
//!
//! The kernel makes one file. `fs_btrfs_link` then gives it more names in
//! the same directory than its `INODE_REF` item for that directory can
//! hold, so the rest must be filed in `INODE_EXTREF` items, keyed by a
//! hash of the parent and the name, as the kernel files them on a volume
//! with the `EXTENDED_IREF` feature. One spilled name is then renamed
//! within the directory through `fs_btrfs_rename`, which drops a name
//! from an extended reference and adds one while the `INODE_REF` is still
//! full. Then:
//!
//! - `btrfs check` calls the volume clean;
//! - `btrfs inspect-internal dump-tree` shows the spilled names in
//!   extended references from the directory;
//! - the guest kernel lists the directory, reports the link count,
//!   counts every back-reference through `inode-resolve` (the paths it
//!   fits, `cnt`, plus those its 4 KiB buffer leaves out, `missed`: about
//!   nineteen 255-byte names fit, #681) with every path it does print a
//!   listed name, and removes some names itself, after which `btrfs
//!   check` looks again.

use fs_btrfs::capi::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, guest_kernel_read_ok, guest_kernel_write_ok, oracle,
    temp_path,
};
use std::ffi::{CStr, CString};
use std::path::PathBuf;

/// `sizeof(struct btrfs_header)`: a leaf's data area is the node less this.
const HEADER: usize = 101;
/// `sizeof(struct btrfs_item)`: the per-item header in a leaf.
const ITEM: usize = 25;
/// `sizeof(struct btrfs_inode_ref)`: `index` (8) and `namelen` (2).
const REF_ENTRY: usize = 10;
/// The volume's node size.
const NODE: usize = 16384;
/// Every link's name length: long, so few names fill the `INODE_REF`.
const LEN: usize = 200;
/// Names past what the `INODE_REF` holds, so some live in extended
/// references.
const SPILL: usize = 4;

/// How many names of `LEN` bytes one `INODE_REF` item holds: the largest
/// item a leaf takes is its data area less one item header.
fn names_that_fit() -> usize {
    (NODE - HEADER - ITEM) / (REF_ENTRY + LEN)
}

/// Link `i`'s name: the index, zero-padded, then padding to `LEN` bytes.
/// Mirrored by `nm` in [`PRELUDE`].
fn name(i: usize) -> String {
    format!("{i:05}{}", "n".repeat(LEN - 5))
}

/// Shell the guest runs first: `nm` builds the same names as [`name`].
const PRELUDE: &str = r#"
cd "$MNT"
LEN=200
nm() { printf '%05d' "$1"; printf '%*s' $((LEN - 5)) '' | tr ' ' n; }
"#;

fn last_error() -> String {
    unsafe { CStr::from_ptr(fs_btrfs_last_error()) }
        .to_string_lossy()
        .into_owned()
}

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

/// One back-reference `dump-tree` prints for the inode under test.
struct BackRef {
    extref: bool,
    parent: u64,
    name: String,
}

/// Every back-reference `dump-tree`'s fs-tree text holds for `ino`.
fn back_refs(dump: &str, ino: u64) -> Vec<BackRef> {
    let mut current: Option<(bool, u64)> = None;
    let mut refs = Vec::new();
    for line in dump.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("item ") {
            current = rest.split_once("key (").and_then(|(_, key)| {
                let mut words = key.trim_end_matches(|c| c != ')').split([' ', ')']);
                let objectid: u64 = words.next()?.parse().ok()?;
                let kind = words.next()?;
                let offset: u64 = words.next()?.parse().ok()?;
                match (objectid == ino, kind) {
                    (true, "INODE_REF") => Some((false, offset)),
                    (true, "INODE_EXTREF") => Some((true, 0)),
                    _ => None,
                }
            });
            continue;
        }
        let Some((extref, key_parent)) = current else {
            continue;
        };
        if !line.starts_with("index ") {
            current = None;
            continue;
        }
        let (fields, name) = line
            .split_once(" name: ")
            .unwrap_or_else(|| panic!("a back-reference with no name: {line:?}"));
        let parent = if extref {
            let words: Vec<&str> = fields.split_whitespace().collect();
            let at = words
                .iter()
                .position(|w| *w == "parent")
                .unwrap_or_else(|| panic!("an INODE_EXTREF entry with no parent: {line:?}"));
            words[at + 1].parse().expect("the extref's parent")
        } else {
            key_parent
        };
        refs.push(BackRef {
            extref,
            parent,
            name: name.to_string(),
        });
    }
    refs
}

#[test]
fn links_past_a_full_inode_ref_are_extended_references_the_kernel_reads() {
    let fit = names_that_fit();
    let total = fit + SPILL;
    let dir = PathBuf::from(temp_path!("link-extref"));
    std::fs::create_dir_all(&dir).unwrap();
    let image = dir.join("fs.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(256 << 20))
        .unwrap();
    let made = oracle("mkfs.btrfs")
        .args(["-q", "-f", "-O", "extref", "-s", "4096", "-n", "16384"])
        .arg(&image)
        .output();
    assert!(
        made.status.success(),
        "mkfs.btrfs: {}",
        String::from_utf8_lossy(&made.stderr)
    );
    let img = image.to_string_lossy().into_owned();
    guest_kernel_write_ok(
        &img,
        "the kernel makes the file to link",
        &format!("{PRELUDE}mkdir d\necho hello > \"d/$(nm 0)\"\nsync\n"),
    );
    let seen = guest_kernel_read_ok(
        &img,
        "the file's and the directory's inode numbers",
        &format!("{PRELUDE}stat -c %i \"d/$(nm 0)\" d\n"),
    );
    let inos: Vec<u64> = seen
        .lines()
        .map(|l| l.trim().parse().expect("an inode number"))
        .collect();
    let (ino, d) = (inos[0], inos[1]);

    // Every name past `fit` must go into an extended reference. The last
    // one is then renamed in place: dropped from its extended reference
    // and filed again while the `INODE_REF` is still full.
    let renamed = name(total);
    let device = c(&img);
    let fs = unsafe { fs_btrfs_mount_rw(device.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_error());
    let mut failed = None;
    for i in 1..total {
        let rc = unsafe {
            fs_btrfs_link(
                fs,
                c(&format!("/d/{}", name(0))).as_ptr(),
                c(&format!("/d/{}", name(i))).as_ptr(),
            )
        };
        if rc != 0 {
            failed = Some(format!("link {i} of {}: {}", total - 1, last_error()));
            break;
        }
    }
    let moved = if failed.is_none() {
        let rc = unsafe {
            fs_btrfs_rename(
                fs,
                c(&format!("/d/{}", name(total - 1))).as_ptr(),
                c(&format!("/d/{renamed}")).as_ptr(),
            )
        };
        (rc == 0).then_some(()).ok_or_else(last_error)
    } else {
        Ok(())
    };
    unsafe { fs_btrfs_umount(fs) };
    if let Some(why) = failed {
        panic!("{why}");
    }
    moved.unwrap_or_else(|why| panic!("rename of a spilled name within its directory: {why}"));

    assert_btrfs_check_clean(&image, "after links spilled into extended references");

    let refs = back_refs(&dump_tree(&image, "5"), ino);
    assert_eq!(refs.len(), total, "dump-tree lists every name");
    assert!(
        refs.iter().all(|r| r.parent == d),
        "a back-reference names a parent other than the directory"
    );
    let spilled: Vec<&str> = refs
        .iter()
        .filter(|r| r.extref)
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(
        spilled.len(),
        SPILL,
        "the names past a full INODE_REF are not all in INODE_EXTREF items"
    );
    for i in fit..total - 1 {
        assert!(
            spilled.contains(&name(i).as_str()),
            "link {i} is not in an INODE_EXTREF item"
        );
    }
    assert!(
        spilled.contains(&renamed.as_str()),
        "the renamed name is not in an INODE_EXTREF item"
    );
    assert!(
        !refs.iter().any(|r| r.name == name(total - 1)),
        "the name renamed away still has a back-reference"
    );

    let seen = guest_kernel_read_ok(
        &img,
        "after the ABI's links and rename",
        &format!(
            "{PRELUDE}stat -c %h \"d/$(nm 0)\"\n\
             ls d | wc -l\n\
             cat \"d/$(nm {total})\"\n\
             test -e \"d/$(nm {})\" && echo old-present || echo old-absent\n\
             btrfs -v inspect-internal inode-resolve \"$(stat -c %i \"d/$(nm 0)\")\" \"$MNT\" \
               > /tmp/raw 2>&1\n\
             sed -n 's/.*cnt=\\([0-9]*\\), missed=\\([0-9]*\\).*/\\1 \\2/p' /tmp/raw \
               | {{ read cnt missed; echo $((cnt + missed)); }}\n\
             grep \"^$MNT/\" /tmp/raw | sed \"s|^$MNT/||\" | LC_ALL=C sort > /tmp/resolved\n\
             ls d | sed 's|^|d/|' | LC_ALL=C sort > /tmp/listed\n\
             LC_ALL=C comm -23 /tmp/resolved /tmp/listed | wc -l\n",
            total - 1
        ),
    );
    let want = format!("{total}\n{total}\nhello\nold-absent\n{total}\n0\n");
    let seen: String = seen.lines().map(|l| format!("{}\n", l.trim())).collect();
    assert_eq!(seen, want, "the kernel sees a different namespace");

    // The kernel removes names we wrote, from both kinds of reference,
    // and the checker looks again.
    guest_kernel_write_ok(
        &img,
        "the kernel unlinks names held in both kinds of reference",
        &format!(
            "{PRELUDE}rm \"d/$(nm 1)\" \"d/$(nm {fit})\" \"d/$(nm {total})\"\n\
             sync\n"
        ),
    );
    assert_btrfs_check_clean(&image, "after the kernel removed names we wrote");
    let seen = guest_kernel_read_ok(
        &img,
        "after the kernel's unlinks",
        &format!("{PRELUDE}stat -c %h \"d/$(nm 0)\"\nls d | wc -l\n"),
    );
    let seen: String = seen.lines().map(|l| format!("{}\n", l.trim())).collect();
    assert_eq!(
        seen,
        format!("{left}\n{left}\n", left = total - 3),
        "the kernel's unlinks left a different count"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
