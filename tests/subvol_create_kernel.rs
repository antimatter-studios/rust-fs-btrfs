//! A subvolume created by this crate, judged by btrfs-progs and the
//! kernel (#267).
//!
//! On a copy of the kernel-made subvolume fixture, this crate creates an
//! empty subvolume `made` at the top and another, `made-in-top`, inside
//! the ordinary directory `top`, each as one committed transaction. (The
//! fixture already has its own `top/nested`, made by the kernel.) Then:
//!
//! - `btrfs check` must find the volume clean;
//! - the UUID tree `btrfs inspect-internal dump-tree` prints must file
//!   each new subvolume under the UUID the kernel reports for it, since
//!   the kernel trusts that index while the superblock says it is current;
//! - the kernel must mount the volume without complaint, list both
//!   subvolumes with the ids, parents and paths this crate gave them,
//!   show their owners and modes, and use `made`: a file written in it,
//!   and a snapshot taken of it;
//! - after the kernel's writes the volume must still be clean, and this
//!   crate must read the kernel's file back out of the subvolume it made.

use std::sync::Arc;

use fs_btrfs::error::Error;
use fs_btrfs::fs::Filesystem;
use fs_btrfs::subvol::FS_TREE_OBJECTID;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, fixture, guest_kernel_probe, guest_kernel_write_ok,
};
use fs_core::{BlockDevice, FileDevice};

/// The subvolume `name` (a path as `btrfs subvolume list` prints it, from
/// this crate's listing, which builds it from names alone).
fn listed(fs: &Filesystem, name: &str) -> Option<fs_btrfs::subvol::Subvolume> {
    fs.subvolumes()
        .expect("list the subvolumes")
        .into_iter()
        .find(|s| s.path == name)
}

/// A UUID as `btrfs subvolume show` prints it, as its sixteen bytes.
fn uuid_bytes(text: &str) -> [u8; 16] {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    assert_eq!(hex.len(), 32, "{text:?} is not a UUID");
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
            .unwrap_or_else(|e| panic!("{text:?} is not a UUID: {e}"));
    }
    out
}

/// A number dump-tree printed, in hex when it says so.
fn number(token: &str) -> u64 {
    let t = token.trim_matches(|c| c == '(' || c == ')');
    match t.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => t.parse(),
    }
    .unwrap_or_else(|e| panic!("{token:?} in a dump-tree key: {e}"))
}

/// Every `UUID_KEY_SUBVOL` item in a dump of the UUID tree, as its key's
/// objectid and offset and the ids it lists.
fn uuid_items(dump: &str) -> Vec<((u64, u64), Vec<u64>)> {
    let mut out: Vec<((u64, u64), Vec<u64>)> = Vec::new();
    for line in dump.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if let Some(at) = words.iter().position(|w| *w == "UUID_KEY_SUBVOL") {
            assert!(
                at >= 1 && at + 1 < words.len(),
                "a UUID tree key this test cannot read: {line:?}"
            );
            out.push(((number(words[at - 1]), number(words[at + 1])), Vec::new()));
        } else if let (Some(&"subvol_id"), Some(id)) = (words.first(), words.get(1)) {
            out.last_mut()
                .unwrap_or_else(|| panic!("a subvol_id before any key: {line:?}"))
                .1
                .push(number(id));
        }
    }
    out
}

#[test]
fn a_created_subvolume_is_one_the_kernel_lists_and_uses() {
    let dir = std::path::PathBuf::from(fs_btrfs_test_support::temp_path!(
        "subvol-create-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    let image = dir.join("btrfs-subvol.img");
    std::fs::copy(fixture("btrfs-subvol.img"), &image).expect("copy the fixture");

    let (made, nested) = {
        let dev = Arc::new(FileDevice::open_rw(&image).expect("open rw"));
        let mut fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount rw");
        let highest = fs
            .subvolumes()
            .expect("list the subvolumes")
            .iter()
            .map(|s| s.id)
            .max()
            .expect("the fixture has subvolumes");
        let root = fs.root_inode().expect("the top directory").ino;
        let top = fs.lookup(root, b"top").expect("the fixture's top").ino;

        let before = fs.superblock().generation;
        let made = fs
            .create_subvolume(root, b"made", 0o755, 0, 0)
            .expect("create made");
        assert_eq!(fs.superblock().generation, before + 1, "one transaction");
        assert_eq!(made, highest + 1, "the next id after the fixture's");
        let nested = fs
            .create_subvolume(top, b"made-in-top", 0o750, 1000, 100)
            .expect("create top/made-in-top");
        assert_eq!(fs.superblock().generation, before + 2, "one transaction");
        assert_eq!(nested, made + 1);

        // This crate's own view: listed, and an empty directory to read.
        let s = listed(&fs, "made").expect("made is listed");
        assert_eq!(
            (s.id, s.parent, s.read_only, s.is_snapshot),
            (made, FS_TREE_OBJECTID, false, false)
        );
        assert_eq!(listed(&fs, "top/made-in-top").map(|s| s.id), Some(nested));
        assert_eq!(fs.subvolume_at(b"/made"), Ok(made));
        assert_eq!(fs.subvolume_at(b"/top/made-in-top"), Ok(nested));
        let target = fs.resolve_path("/made").expect("resolve /made");
        let entries = target
            .fs(&fs)
            .read_dir(target.inode.ino)
            .expect("read made");
        assert!(entries.is_empty(), "made is not empty: {entries:?}");

        // Refused, and nothing committed.
        let generation = fs.superblock().generation;
        assert!(matches!(
            fs.create_subvolume(root, b"made", 0o755, 0, 0),
            Err(Error::UnsupportedFeature(_))
        ));
        assert!(matches!(
            fs.create_subvolume(root, b"..", 0o755, 0, 0),
            Err(Error::UnsupportedFeature(_))
        ));
        assert_eq!(
            fs.superblock().generation,
            generation,
            "a refusal committed"
        );
        (made, nested)
    };

    // A read-only mount writes nothing.
    {
        let dev = Arc::new(FileDevice::open(&image).expect("open"));
        let mut fs = Filesystem::mount(dev).expect("mount");
        let root = fs.root_inode().expect("the top directory").ino;
        assert_eq!(
            fs.create_subvolume(root, b"refused", 0o755, 0, 0),
            Err(Error::ReadOnly)
        );
    }

    assert_btrfs_check_clean(&image, "after the subvolumes were created");
    let uuids = uuid_items(&dump_tree(&image, "uuid"));

    let image_str = image.to_str().expect("a UTF-8 scratch path").to_string();
    let probe = guest_kernel_probe(&image_str, "subvolumes");
    assert!(
        probe.complaints.is_empty(),
        "the kernel complained mounting the volume: {:?}",
        probe.complaints
    );
    let out = guest_kernel_write_ok(
        &image_str,
        "the created subvolumes",
        r#"
cd "$MNT"
btrfs subvolume list -p . | sed 's/^/list /'
for s in made top/made-in-top; do
    btrfs subvolume show "$s" | sed -En "s@^[[:space:]]*(UUID|Subvolume ID|Parent ID|Flags):[[:space:]]*@show $s \1=@p"
    echo "stat $s $(stat -c '%a %u %g' "$s") $(ls -A "$s" | wc -l)"
done
printf %s 'written by the kernel' > made/k.txt
btrfs subvolume snapshot made made-snap >/dev/null
echo "snap $(cat made-snap/k.txt)"
"#,
    );

    let lines: Vec<&str> = out.lines().map(str::trim).collect();
    // `ID 264 gen 9 parent 5 top level 5 path made`
    let list: Vec<(u64, u64, &str)> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("list "))
        .map(|l| {
            let w: Vec<&str> = l.split_whitespace().collect();
            let field = |name: &str| {
                w.iter()
                    .position(|x| *x == name)
                    .and_then(|i| w.get(i + 1))
                    .copied()
                    .unwrap_or_else(|| panic!("no {name} in {l:?}"))
            };
            (number(field("ID")), number(field("parent")), field("path"))
        })
        .collect();
    assert!(
        list.contains(&(made, FS_TREE_OBJECTID, "made")),
        "the kernel's list:\n{out}"
    );
    assert!(
        list.contains(&(nested, FS_TREE_OBJECTID, "top/made-in-top")),
        "the kernel's list:\n{out}"
    );
    let show = |path: &str, field: &str| -> String {
        let prefix = format!("show {path} {field}=");
        lines
            .iter()
            .find_map(|l| l.strip_prefix(prefix.as_str()))
            .unwrap_or_else(|| panic!("no {field} for {path} in:\n{out}"))
            .to_string()
    };
    for (path, id) in [("made", made), ("top/made-in-top", nested)] {
        assert_eq!(show(path, "Subvolume ID"), id.to_string(), "{out}");
        assert_eq!(
            show(path, "Parent ID"),
            FS_TREE_OBJECTID.to_string(),
            "{out}"
        );
        assert_eq!(show(path, "Flags"), "-", "{out}");
        // Filed in the UUID tree under the UUID the kernel reports.
        let u = uuid_bytes(&show(path, "UUID"));
        let key = (
            u64::from_le_bytes(u[..8].try_into().expect("8 bytes")),
            u64::from_le_bytes(u[8..].try_into().expect("8 bytes")),
        );
        assert!(
            uuids.contains(&(key, vec![id])),
            "{path}'s UUID is not filed for subvolume {id} in the UUID tree: {uuids:?}"
        );
    }
    assert!(lines.contains(&"stat made 755 0 0 0"), "{out}");
    assert!(
        lines.contains(&"stat top/made-in-top 750 1000 100 0"),
        "{out}"
    );
    assert!(lines.contains(&"snap written by the kernel"), "{out}");

    // The kernel's writes, on top of ours, are still a clean volume, and
    // the file it wrote is in the subvolume this crate made.
    assert_btrfs_check_clean(&image, "after the kernel wrote to the subvolumes");
    let dev = Arc::new(FileDevice::open(&image).expect("open"));
    let fs = Filesystem::mount(dev).expect("mount");
    let target = fs.resolve_path("/made/k.txt").expect("resolve /made/k.txt");
    assert_eq!(
        target
            .fs(&fs)
            .read_file(target.inode.ino)
            .expect("read k.txt"),
        b"written by the kernel"
    );
    let snap = listed(&fs, "made-snap").expect("the kernel's snapshot is listed");
    assert!(snap.is_snapshot, "made-snap is not a snapshot: {snap:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
