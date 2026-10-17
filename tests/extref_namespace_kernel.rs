//! Names held in extended references, removed and moved through the C
//! ABI, judged by what is not this crate (#424).
//!
//! A file with many hard links in one directory spills the names its
//! `INODE_REF` item for that directory cannot hold into `INODE_EXTREF`
//! items, keyed by a hash of the parent and the name. The guest kernel
//! makes such a file; `dump-tree` confirms which names spilled. Then one
//! spilled name is unlinked through `fs_btrfs_unlink` and another moved to
//! a second directory through `fs_btrfs_rename`, and:
//!
//! - `btrfs check` calls the volume clean — the link count, the extended
//!   reference item and both directory entries of every name;
//! - the guest kernel lists the directory and reports the link count;
//! - `btrfs inspect-internal inode-resolve`, the kernel's own walk of the
//!   back-references, resolves exactly the names that remain.

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

/// How many names of `LEN` bytes the one `INODE_REF` item holds before
/// the next link spills (see `tests/hard_link_extref.rs`).
fn names_that_fit() -> usize {
    (NODE - HEADER - 2 * ITEM) / (REF_ENTRY + LEN)
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
fn spilled_names_are_unlinked_and_renamed_and_the_kernel_agrees() {
    let fit = names_that_fit();
    let total = fit + SPILL;
    let dir = PathBuf::from(temp_path!("extref-namespace"));
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
        "the kernel links one file past its INODE_REF",
        &format!(
            "{PRELUDE}mkdir d other\n\
             echo hello > \"d/$(nm 0)\"\n\
             for i in $(seq 1 {}); do ln \"d/$(nm 0)\" \"d/$(nm \"$i\")\"; done\n\
             sync\n",
            total - 1
        ),
    );

    // The names past `fit` are in extended references, as the kernel
    // filed them, and the two this test moves are among them.
    let seen = guest_kernel_read_ok(
        &img,
        "the file's and the directories' inode numbers",
        &format!("{PRELUDE}stat -c %i \"d/$(nm 0)\" d other\n"),
    );
    let inos: Vec<u64> = seen
        .lines()
        .map(|l| l.trim().parse().expect("an inode number"))
        .collect();
    let (ino, d, other) = (inos[0], inos[1], inos[2]);
    let refs = back_refs(&dump_tree(&image, "5"), ino);
    assert_eq!(refs.len(), total, "dump-tree lists every name");
    let gone = name(total - 2);
    let moved = name(total - 1);
    for n in [&gone, &moved] {
        assert!(
            refs.iter()
                .any(|r| r.extref && r.parent == d && &r.name == n),
            "{n:?} is not in an INODE_EXTREF item, so this test proves nothing"
        );
    }

    // One spilled name unlinked, another moved to the second directory.
    let device = c(&img);
    let fs = unsafe { fs_btrfs_mount_rw(device.as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_error());
    let rc = unsafe { fs_btrfs_unlink(fs, c(&format!("/d/{gone}")).as_ptr()) };
    let unlinked = last_error();
    let rc2 = unsafe {
        fs_btrfs_rename(
            fs,
            c(&format!("/d/{moved}")).as_ptr(),
            c("/other/moved").as_ptr(),
        )
    };
    let renamed = last_error();
    unsafe { fs_btrfs_umount(fs) };
    assert_eq!(rc, 0, "unlink of a spilled name: {unlinked}");
    assert_eq!(rc2, 0, "rename of a spilled name: {renamed}");

    assert_btrfs_check_clean(&image, "after spilled names were unlinked and renamed");

    let refs = back_refs(&dump_tree(&image, "5"), ino);
    assert!(
        !refs.iter().any(|r| r.name == gone || r.name == moved),
        "a removed name still has a back-reference"
    );
    assert!(
        refs.iter()
            .any(|r| !r.extref && r.parent == other && r.name == "moved"),
        "the moved name has no INODE_REF from the second directory"
    );
    assert_eq!(
        refs.len(),
        total - 1,
        "dump-tree lists the names that remain"
    );

    let seen = guest_kernel_read_ok(
        &img,
        "after the ABI's unlink and rename",
        &format!(
            "{PRELUDE}stat -c %h \"d/$(nm 0)\"\n\
             ls d | wc -l\n\
             ls other\n\
             cat other/moved\n\
             test -e \"d/$(nm {})\" && echo gone-present || echo gone-absent\n\
             test -e \"d/$(nm {})\" && echo moved-present || echo moved-absent\n\
             btrfs inspect-internal inode-resolve \"$(stat -c %i other/moved)\" \"$MNT\" \
               | sed \"s|^$MNT/||\" | LC_ALL=C sort > /tmp/resolved\n\
             wc -l < /tmp/resolved\n\
             {{ ls d | sed 's|^|d/|'; echo other/moved; }} | LC_ALL=C sort \
               | cmp -s - /tmp/resolved && echo resolve-matches || cat /tmp/resolved\n",
            total - 2,
            total - 1
        ),
    );
    let want = format!(
        "{remain}\n{in_d}\nmoved\nhello\ngone-absent\nmoved-absent\n{remain}\nresolve-matches\n",
        remain = total - 1,
        in_d = total - 2,
    );
    let seen: String = seen.lines().map(|l| format!("{}\n", l.trim())).collect();
    assert_eq!(seen, want, "the kernel sees a different namespace");

    // The kernel builds on what we wrote, and the checker looks again.
    guest_kernel_write_ok(
        &img,
        "the kernel unlinks and links over our references",
        &format!(
            "{PRELUDE}rm \"d/$(nm {})\"\n\
             ln other/moved \"d/$(nm {})\"\n\
             sync\n",
            total - 3,
            total - 2
        ),
    );
    assert_btrfs_check_clean(&image, "after the kernel changed the references we wrote");
    let _ = std::fs::remove_dir_all(&dir);
}
