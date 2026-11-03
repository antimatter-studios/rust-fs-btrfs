//! Snapshots of the shapes `btrfs_inc_ref` has more than one answer for,
//! judged by btrfs-progs and the kernel (#700): a source tree of level one
//! or more, whose root's children each gain a reference from the snapshot,
//! and a source leaf whose data extents keep references as items of their
//! own, or have to start.
//!
//! Each test first has the kernel take the same snapshot of a copy of the
//! image and holds that result to the shape this crate must write, so what
//! is asserted is what the kernel does. Then this crate takes the snapshot,
//! and:
//!
//! - `btrfs check` must find the volume clean, and `dump-tree` must show
//!   the references where the kernel's own were;
//! - the kernel must read every file of both trees as the bytes this test
//!   knows they hold;
//! - the kernel must write into each side, copying what they share, with
//!   each side keeping its own bytes and the volume left clean;
//! - the kernel must delete one side and clean it up, walking every
//!   reference this crate wrote, with the other side intact and the volume
//!   still clean.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs_btrfs::fs::Filesystem;
use fs_btrfs::mkfs;
use fs_btrfs::subvol::FS_TREE_OBJECTID;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, guest_kernel_read_ok, guest_kernel_write_ok, oracle,
    sha256_hex,
};
use fs_core::{BlockDevice, FileDevice};

/// How many files fill a tree past one leaf, and past one node's first
/// few children.
const FILES: usize = 300;

/// Bytes that are not all one value, so a misplaced block shows.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// A scratch directory holding an empty formatted image.
struct Scratch {
    dir: PathBuf,
    image: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let scratch = Self::empty(tag, 256 << 20);
        let dev = FileDevice::open_rw(&scratch.image).expect("open rw");
        mkfs::format(&dev, &mkfs::Options::default()).expect("format");
        scratch
    }

    /// A volume `mkfs.btrfs` makes on 4 KiB nodes, large enough that its
    /// metadata block group's free-space records stay extents through the
    /// kernel's commits: a small one is a bitmap within a few of them, and
    /// no write here takes a bitmap.
    fn small_nodes(tag: &str) -> Self {
        let scratch = Self::empty(tag, 2 << 30);
        let made = oracle("mkfs.btrfs")
            .args(["-q", "-f", "-s", "4096", "-n", "4096"])
            .arg(&scratch.image)
            .output();
        assert!(
            made.status.success(),
            "mkfs.btrfs: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        scratch
    }

    /// An unformatted image of `len` bytes.
    fn empty(tag: &str, len: u64) -> Self {
        let dir = PathBuf::from(fs_btrfs_test_support::temp_path!(
            "subvol-snapshot-tall-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the scratch directory");
        let image = dir.join("snapshot.img");
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(len))
            .expect("create the image");
        Self { dir, image }
    }

    fn mount(&self) -> Filesystem {
        let dev = Arc::new(FileDevice::open_rw(&self.image).expect("open rw"));
        Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount rw")
    }

    fn mount_ro(&self) -> Filesystem {
        let dev = Arc::new(FileDevice::open(&self.image).expect("open"));
        Filesystem::mount(dev).expect("mount")
    }

    fn image_str(&self) -> String {
        self.image
            .to_str()
            .expect("a UTF-8 scratch path")
            .to_string()
    }

    /// A copy of the image for the kernel to take its own snapshot in.
    fn copy(&self, name: &str) -> String {
        let copy = self.dir.join(name);
        std::fs::File::create(&copy).expect("create the copy");
        let copy = copy.to_str().expect("a UTF-8 scratch path").to_string();
        guest_kernel_read_ok(
            &self.image_str(),
            "copying the image",
            &format!("cp --sparse=always \"$IMG\" {copy}\n"),
        );
        copy
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// What a tree full of files holds, by name: `big` with an extent of its
/// own, every eighth file with one too, and the rest inline.
fn files() -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    out.insert("big".to_string(), pattern(192 << 10, 7));
    for i in 0..FILES {
        let len = if i % 8 == 0 { 8192 } else { 64 };
        out.insert(format!("f{i:04}"), pattern(len, i as u8));
    }
    out
}

/// Put `files` in the top directory of the tree `fs` writes.
fn fill(fs: &mut Filesystem, files: &BTreeMap<String, Vec<u8>>) {
    let root = fs.root_inode().unwrap().ino;
    for (name, data) in files {
        let ino = fs.create(root, name.as_bytes(), 0o644, 0, 0).unwrap();
        fs.write(ino, 0, data).unwrap();
    }
}

/// What the kernel's script below prints for a tree holding `files`: the
/// digest of `sha256sum`'s listing of them, in byte order of `./name`.
fn listing(files: &BTreeMap<String, Vec<u8>>) -> String {
    let mut text = String::new();
    for (name, data) in files {
        text.push_str(&format!("{}  ./{name}\n", sha256_hex(data)));
    }
    sha256_hex(text.as_bytes())
}

/// Shell printing `sum-<tree> <digest>` for a directory of the mount: every
/// file under it, without crossing into another subvolume.
fn sum(tree: &str) -> String {
    format!(
        "printf 'sum-%s ' {tree}\n(cd {tree} && find . -xdev -type f -print0 | LC_ALL=C sort -z \
         | xargs -0 sha256sum | sha256sum)\n"
    )
}

/// The digest `sum` printed for `tree`.
#[track_caller]
fn kernel_sum<'a>(out: &'a str, tree: &str) -> &'a str {
    let tag = format!("sum-{tree} ");
    out.lines()
        .filter_map(|l| l.trim().strip_prefix(tag.as_str()))
        .filter_map(|l| l.split_whitespace().next())
        .next_back()
        .unwrap_or_else(|| panic!("no digest of {tree} in the kernel's output:\n{out}"))
}

/// Hold the kernel's reading of `tree` to `files`.
#[track_caller]
fn assert_kernel_read(out: &str, tree: &str, files: &BTreeMap<String, Vec<u8>>) {
    assert_eq!(
        kernel_sum(out, tree),
        listing(files),
        "the kernel's reading of {tree}:\n{out}"
    );
}

/// The line `dump-tree` opens a tree with, and whether that root is a node.
#[track_caller]
fn root_is_node(dump: &str) -> bool {
    let first = dump
        .lines()
        .find(|l| l.starts_with("node ") || l.starts_with("leaf "))
        .unwrap_or_else(|| panic!("no block in the dump:\n{dump}"));
    first.starts_with("node ")
}

/// What the root of tree `id` points at, from `dump-tree`.
#[track_caller]
fn root_children(image: &Path, id: u64) -> Vec<u64> {
    let dump = dump_tree(image, &id.to_string());
    assert!(root_is_node(&dump), "tree {id}'s root is a leaf:\n{dump}");
    let next_block =
        |l: &&str| l.starts_with("leaf ") || (l.starts_with("node ") && l.contains(" level "));
    let lines = dump
        .lines()
        .skip_while(|l| !l.starts_with("node "))
        .skip(1)
        .take_while(|l| !next_block(l));
    let mut out = Vec::new();
    for line in lines {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.first() == Some(&"key") {
            let at = words
                .iter()
                .position(|w| *w == "block")
                .and_then(|i| words.get(i + 1)?.parse().ok())
                .unwrap_or_else(|| panic!("no block in {line:?}"));
            out.push(at);
        }
    }
    assert!(!out.is_empty(), "tree {id}'s root has no children:\n{dump}");
    out
}

/// A tree block's references, from `dump-tree -t extent`.
#[derive(Debug, Default, PartialEq, Eq)]
struct BlockRecord {
    /// The record's total.
    total: u64,
    /// Trees named inline.
    inline: Vec<String>,
    /// Blocks named inline, by address.
    shared: Vec<u64>,
    /// Trees named by a `TREE_BLOCK_REF` item of their own.
    keyed: Vec<String>,
}

impl BlockRecord {
    /// Whether tree `id` refers to the block, either way.
    fn names(&self, id: u64) -> bool {
        let id = id.to_string();
        self.inline.contains(&id) || self.keyed.contains(&id)
    }

    /// How many references it lists, against its total.
    fn listed(&self) -> u64 {
        (self.inline.len() + self.shared.len() + self.keyed.len()) as u64
    }
}

/// The items of `dump-tree -t extent` whose keys name `bytenr`, each with
/// the lines of its body.
fn items_of(extents: &str, bytenr: u64) -> Vec<(&str, Vec<&str>)> {
    let tag = format!(" key ({bytenr} ");
    let mut out: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut inside = false;
    for line in extents.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("item ") {
            inside = line.contains(&tag);
            if inside {
                out.push((line, Vec::new()));
            }
        } else if trimmed.starts_with("leaf ") || trimmed.starts_with("node ") {
            inside = false;
        } else if inside {
            if let Some(last) = out.last_mut() {
                last.1.push(line);
            }
        }
    }
    out
}

/// The word after `name` in `line`.
fn word_after<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let words: Vec<&str> = line.split_whitespace().collect();
    words
        .iter()
        .position(|w| *w == name)
        .and_then(|i| words.get(i + 1).copied())
}

/// The record of the tree block at `bytenr`.
#[track_caller]
fn block_record(extents: &str, bytenr: u64) -> BlockRecord {
    let mut out = BlockRecord::default();
    let mut found = false;
    for (key, body) in items_of(extents, bytenr) {
        if key.contains(" METADATA_ITEM ") {
            found = true;
            out.total = body
                .iter()
                .find_map(|l| word_after(l, "refs")?.parse().ok())
                .unwrap_or_else(|| panic!("no total in {body:?}"));
            for line in body {
                if line.contains("tree block backref") {
                    let root =
                        word_after(line, "root").unwrap_or_else(|| panic!("no root in {line:?}"));
                    out.inline.push(root.to_string());
                } else if line.contains("shared block backref") {
                    let parent = word_after(line, "parent")
                        .and_then(|p| p.parse().ok())
                        .unwrap_or_else(|| panic!("no parent in {line:?}"));
                    out.shared.push(parent);
                }
            }
        } else if key.contains(" TREE_BLOCK_REF ") {
            let root = word_after(key, "TREE_BLOCK_REF")
                .map(|r| r.trim_end_matches(')'))
                .unwrap_or_else(|| panic!("no root in {key:?}"));
            out.keyed.push(root.to_string());
        } else if key.contains(" SHARED_BLOCK_REF ") {
            let parent = word_after(key, "SHARED_BLOCK_REF")
                .map(|r| r.trim_end_matches(')'))
                .and_then(|p| p.parse().ok())
                .unwrap_or_else(|| panic!("no parent in {key:?}"));
            out.shared.push(parent);
        }
    }
    assert!(found, "no record of the tree block at {bytenr}");
    out
}

/// Tree `id`'s references of the data extent at `bytenr`: its record's
/// total, then how many of them are counted inline and how many in items
/// of their own.
#[track_caller]
fn data_refs(extents: &str, bytenr: u64, id: u64) -> (u64, u64, u64) {
    let id = id.to_string();
    let (mut total, mut inline, mut keyed) = (None, 0, 0);
    for (key, body) in items_of(extents, bytenr) {
        let is_record = key.contains(" EXTENT_ITEM ");
        if is_record {
            total = body
                .iter()
                .find_map(|l| word_after(l, "refs")?.parse().ok());
        }
        for line in body {
            if !line.contains("extent data backref")
                || word_after(line, "root") != Some(id.as_str())
            {
                continue;
            }
            let count: u64 = word_after(line, "count")
                .and_then(|c| c.parse().ok())
                .unwrap_or_else(|| panic!("no count in {line:?}"));
            if is_record {
                inline += count;
            } else {
                keyed += count;
            }
        }
    }
    let total = total.unwrap_or_else(|| panic!("no record of the data extent at {bytenr}"));
    (total, inline, keyed)
}

/// Every reference inline or in an item of its own, whichever tree it is
/// from, of the data extent at `bytenr`.
#[track_caller]
fn inline_and_keyed(extents: &str, bytenr: u64) -> (u64, u64) {
    let (mut inline, mut keyed) = (0, 0);
    for (key, body) in items_of(extents, bytenr) {
        let is_record = key.contains(" EXTENT_ITEM ");
        for line in body {
            if !line.contains("extent data backref") {
                continue;
            }
            if is_record {
                inline += 1;
            } else {
                keyed += 1;
            }
        }
    }
    (inline, keyed)
}

/// The address of the data extent the first file item of tree `id` names.
#[track_caller]
fn first_data_extent(image: &Path, id: u64) -> u64 {
    let dump = dump_tree(image, &id.to_string());
    dump.lines()
        .find_map(|l| {
            l.contains("extent data disk byte")
                .then(|| word_after(l, "byte")?.parse().ok())
                .flatten()
        })
        .unwrap_or_else(|| panic!("no data extent in tree {id}:\n{dump}"))
}

/// Hold every child of tree `id`'s root to what `btrfs_inc_ref` leaves: a
/// reference naming `id`, and a total counting what the record lists.
#[track_caller]
fn assert_children_name(image: &Path, id: u64) {
    let extents = dump_tree(image, "2");
    for child in root_children(image, id) {
        let record = block_record(&extents, child);
        assert!(
            record.names(id),
            "the child at {child} of tree {id} has no reference from it: {record:?}"
        );
        assert_eq!(
            record.total,
            record.listed(),
            "the child at {child} of tree {id}: {record:?}"
        );
    }
}

/// The id the kernel's script printed after `tag`.
#[track_caller]
fn printed_id(out: &str, tag: &str) -> u64 {
    out.lines()
        .filter_map(|l| l.trim().strip_prefix(tag))
        .find_map(|l| l.trim().parse().ok())
        .unwrap_or_else(|| panic!("no {tag} in the kernel's output:\n{out}"))
}

/// Hold this crate's reading of `tree` to `files`.
#[track_caller]
fn assert_read_here(fs: &Filesystem, tree: &str, files: &BTreeMap<String, Vec<u8>>) {
    let root = fs.root_inode().unwrap().ino;
    for (name, data) in files {
        let ino = fs
            .lookup(root, name.as_bytes())
            .unwrap_or_else(|e| panic!("{name} in {tree}: {e:?}"))
            .ino;
        assert!(
            fs.read_file(ino).unwrap() == *data,
            "{name} in {tree} read back differently"
        );
    }
}

/// The kernel's script appending to every file of `tree` and overwriting
/// the middle of `big`, with what each side should then hold.
fn scribble(tree: &str, files: &BTreeMap<String, Vec<u8>>) -> (String, BTreeMap<String, Vec<u8>>) {
    let mut script = String::new();
    let mut after = files.clone();
    for (name, data) in after.iter_mut() {
        if name == "big" {
            continue;
        }
        let tail = format!("{tree}-{name}");
        script.push_str(&format!("printf '{tail}' >> {tree}/{name}\n"));
        data.extend_from_slice(tail.as_bytes());
    }
    let mark = format!("{tree:8.8}");
    script.push_str(&format!(
        "printf '{mark}' | dd of={tree}/big bs=1 seek=4096 conv=notrunc status=none\n"
    ));
    if let Some(big) = after.get_mut("big") {
        big[4096..4104].copy_from_slice(mark.as_bytes());
    }
    (script, after)
}

/// A source tree of level one or more: only its root is copied, and each
/// child gains a reference from the snapshot, whatever it already holds. A
/// snapshot of that snapshot does the same to blocks another tree owns.
#[test]
fn a_tall_subvolume_is_snapshotted() {
    let scratch = Scratch::new("subvolume");
    let contents = files();
    let tall = {
        let mut fs = scratch.mount();
        let root = fs.root_inode().unwrap().ino;
        let tall = fs.create_subvolume(root, b"tall", 0o755, 0, 0).unwrap();
        fill(&mut fs.open_subvolume(tall).unwrap(), &contents);
        tall
    };
    assert_btrfs_check_clean(&scratch.image, "with the tall subvolume filled");

    // The kernel's snapshot of the same tree, on a copy.
    let measured = scratch.copy("measured.img");
    let out = guest_kernel_write_ok(
        &measured,
        "the kernel's snapshot",
        r#"
cd "$MNT"
btrfs subvolume snapshot tall ksnap >/dev/null
printf 'ksnap-id %s\n' "$(btrfs inspect-internal rootid ksnap)"
"#,
    );
    let measured = PathBuf::from(measured);
    let ksnap = printed_id(&out, "ksnap-id ");
    assert_children_name(&measured, ksnap);
    assert_btrfs_check_clean(&measured, "after the kernel's snapshot");

    let (snap, snap2) = {
        let mut fs = scratch.mount();
        let root = fs.root_inode().unwrap().ino;
        let generation = fs.superblock().generation;
        let snap = fs
            .create_snapshot(tall, root, b"snap", false)
            .expect("snapshot a tree taller than a leaf");
        assert_eq!(
            fs.superblock().generation,
            generation + 1,
            "one transaction"
        );
        let snap2 = fs
            .create_snapshot(snap, root, b"snap2", true)
            .expect("snapshot a snapshot sharing its source's blocks");
        (snap, snap2)
    };
    assert_btrfs_check_clean(&scratch.image, "after the snapshots were taken");

    // The roots are copies, and every child has a reference from each.
    let children = root_children(&scratch.image, tall);
    assert_eq!(root_children(&scratch.image, snap), children);
    assert_eq!(root_children(&scratch.image, snap2), children);
    assert_children_name(&scratch.image, tall);
    assert_children_name(&scratch.image, snap);
    assert_children_name(&scratch.image, snap2);

    let script = format!(
        r#"cd "$MNT"
{tall}{snap}{snap2}btrfs subvolume snapshot tall ksnap >/dev/null
"#,
        tall = sum("tall"),
        snap = sum("snap"),
        snap2 = sum("snap2"),
    );
    let out = guest_kernel_write_ok(&scratch.image_str(), "reading the snapshots", &script);
    for tree in ["tall", "snap", "snap2"] {
        assert_kernel_read(&out, tree, &contents);
    }
    assert_btrfs_check_clean(&scratch.image, "after the kernel snapshotted one as well");

    // Writes into each, every leaf of each.
    let (write_tall, tall_after) = scribble("tall", &contents);
    let (write_snap, snap_after) = scribble("snap", &contents);
    let script = format!(
        r#"cd "$MNT"
{write_snap}{write_tall}sync
if printf 'x' > snap2/new 2>/dev/null; then echo snap2-written; else echo snap2-refused; fi
{tall}{snap}{snap2}{ksnap}"#,
        tall = sum("tall"),
        snap = sum("snap"),
        snap2 = sum("snap2"),
        ksnap = sum("ksnap"),
    );
    let out = guest_kernel_write_ok(&scratch.image_str(), "writing into both sides", &script);
    assert_kernel_read(&out, "tall", &tall_after);
    assert_kernel_read(&out, "snap", &snap_after);
    assert_kernel_read(&out, "snap2", &contents);
    assert_kernel_read(&out, "ksnap", &contents);
    assert!(
        out.lines().any(|l| l.trim() == "snap2-refused"),
        "the kernel wrote into the read-only snapshot:\n{out}"
    );
    assert_btrfs_check_clean(&scratch.image, "after the kernel wrote into both sides");

    {
        let fs = scratch.mount_ro();
        assert_read_here(&fs.open_subvolume(tall).unwrap(), "tall", &tall_after);
        assert_read_here(&fs.open_subvolume(snap).unwrap(), "snap", &snap_after);
        assert_read_here(&fs.open_subvolume(snap2).unwrap(), "snap2", &contents);
    }

    // The kernel walks every reference, dropping the trees that hold them.
    let script = format!(
        r#"cd "$MNT"
btrfs subvolume delete tall >/dev/null
btrfs subvolume delete snap >/dev/null
btrfs subvolume sync . >/dev/null && echo synced
{snap2}{ksnap}"#,
        snap2 = sum("snap2"),
        ksnap = sum("ksnap"),
    );
    let out = guest_kernel_write_ok(&scratch.image_str(), "deleting two sides", &script);
    assert!(
        out.lines().any(|l| l.trim() == "synced"),
        "the cleaner did not finish:\n{out}"
    );
    assert_kernel_read(&out, "snap2", &contents);
    assert_kernel_read(&out, "ksnap", &contents);
    assert_btrfs_check_clean(&scratch.image, "after the kernel's cleaner dropped two");
}

/// The top level as the source: the snapshot's entry goes in the tree being
/// copied, so the blocks on the way to it are copied as the kernel copies
/// them once the snapshot shares them, and the snapshot does not hold its
/// own entry.
#[test]
fn a_tall_top_level_is_snapshotted() {
    let scratch = Scratch::new("top");
    let contents = files();
    fill(&mut scratch.mount(), &contents);
    assert_btrfs_check_clean(&scratch.image, "with the top level filled");

    let measured = scratch.copy("measured.img");
    let out = guest_kernel_write_ok(
        &measured,
        "the kernel's snapshot",
        r#"
cd "$MNT"
btrfs subvolume snapshot . ksnap >/dev/null
printf 'ksnap-id %s\n' "$(btrfs inspect-internal rootid ksnap)"
"#,
    );
    let measured = PathBuf::from(measured);
    assert_children_name(&measured, printed_id(&out, "ksnap-id "));
    assert_btrfs_check_clean(&measured, "after the kernel's snapshot");

    let snap = {
        let mut fs = scratch.mount();
        let root = fs.root_inode().unwrap().ino;
        fs.create_snapshot(FS_TREE_OBJECTID, root, b"snap", false)
            .expect("snapshot a top level taller than a leaf")
    };
    assert_btrfs_check_clean(&scratch.image, "after the snapshot was taken");
    assert_children_name(&scratch.image, snap);

    let out = guest_kernel_read_ok(
        &scratch.image_str(),
        "reading the snapshot",
        &format!("cd \"$MNT\"\n{}{}", sum("."), sum("snap")),
    );
    assert_kernel_read(&out, ".", &contents);
    assert_kernel_read(&out, "snap", &contents);

    let (write_snap, snap_after) = scribble("snap", &contents);
    let mut top_after = contents.clone();
    let mut write_top = String::new();
    for (name, data) in top_after.iter_mut() {
        if name != "big" {
            write_top.push_str(&format!("printf 'top-{name}' >> {name}\n"));
            data.extend_from_slice(format!("top-{name}").as_bytes());
        }
    }
    let script = format!(
        r#"cd "$MNT"
{write_snap}{write_top}sync
{top}{snap}"#,
        top = sum("."),
        snap = sum("snap"),
    );
    let out = guest_kernel_write_ok(&scratch.image_str(), "writing into both sides", &script);
    assert_kernel_read(&out, ".", &top_after);
    assert_kernel_read(&out, "snap", &snap_after);
    assert_btrfs_check_clean(&scratch.image, "after the kernel wrote into both sides");

    let script = format!(
        r#"cd "$MNT"
btrfs subvolume delete snap >/dev/null
btrfs subvolume sync . >/dev/null && echo synced
{top}"#,
        top = sum("."),
    );
    let out = guest_kernel_write_ok(&scratch.image_str(), "deleting the snapshot", &script);
    assert!(
        out.lines().any(|l| l.trim() == "synced"),
        "the cleaner did not finish:\n{out}"
    );
    assert_kernel_read(&out, ".", &top_after);
    assert_btrfs_check_clean(&scratch.image, "after the kernel's cleaner dropped it");
}

/// A write through this crate into the source, after the snapshot shares
/// its blocks: the source's copy changes, and the snapshot still reads
/// what it held.
#[test]
fn a_write_here_into_a_tall_source_leaves_its_snapshot_alone() {
    let scratch = Scratch::new("write-here");
    let contents = files();
    let (tall, snap) = {
        let mut fs = scratch.mount();
        let root = fs.root_inode().unwrap().ino;
        let tall = fs.create_subvolume(root, b"tall", 0o755, 0, 0).unwrap();
        fill(&mut fs.open_subvolume(tall).unwrap(), &contents);
        // The fill committed past `fs`: mount afresh to snapshot (#599).
        drop(fs);
        let mut fs = scratch.mount();
        let snap = fs.create_snapshot(tall, root, b"snap", false).unwrap();
        (tall, snap)
    };
    let mut changed = contents.clone();
    {
        let fs = scratch.mount();
        let mut sv = fs.open_subvolume(tall).unwrap();
        let root = sv.root_inode().unwrap().ino;
        let ino = sv.lookup(root, b"f0008").unwrap().ino;
        sv.write(ino, 0, b"changed in the source").unwrap();
        changed.get_mut("f0008").unwrap()[..21].copy_from_slice(b"changed in the source");
    }
    assert_btrfs_check_clean(&scratch.image, "after this crate wrote into the source");
    {
        let fs = scratch.mount_ro();
        assert_read_here(&fs.open_subvolume(tall).unwrap(), "tall", &changed);
        assert_read_here(&fs.open_subvolume(snap).unwrap(), "snap", &contents);
    }
    let out = guest_kernel_read_ok(
        &scratch.image_str(),
        "reading both sides",
        &format!("cd \"$MNT\"\n{}{}", sum("tall"), sum("snap")),
    );
    assert_kernel_read(&out, "tall", &changed);
    assert_kernel_read(&out, "snap", &contents);
}

/// A subvolume whose one leaf names one data extent `reflinks` times over,
/// from its original file `o` and its copies `r01` on, as the kernel files
/// those references. Returns the subvolume's id, the extent's address and
/// what the tree holds.
fn reflinked(scratch: &Scratch, reflinks: usize) -> (u64, u64, BTreeMap<String, Vec<u8>>) {
    let data = pattern(64 << 10, 41);
    let source = scratch.dir.join("source");
    std::fs::write(&source, &data).expect("write the source bytes");
    let mut script = format!(
        r#"cd "$MNT"
btrfs subvolume create sub >/dev/null
cat {source} > sub/o
sync
"#,
        source = source.display()
    );
    let mut contents = BTreeMap::new();
    contents.insert("o".to_string(), data.clone());
    for i in 1..=reflinks {
        script.push_str(&format!("cp --reflink=always sub/o sub/r{i:02}\n"));
        contents.insert(format!("r{i:02}"), data.clone());
    }
    script.push_str("sync\nprintf 'sub-id %s\\n' \"$(btrfs inspect-internal rootid sub)\"\n");
    let out = guest_kernel_write_ok(&scratch.image_str(), "the kernel's reflinks", &script);
    let sub = printed_id(&out, "sub-id ");
    let dump = dump_tree(&scratch.image, &sub.to_string());
    assert!(
        !root_is_node(&dump),
        "the premise: tree {sub}'s root is a leaf:\n{dump}"
    );
    let extent = first_data_extent(&scratch.image, sub);
    (sub, extent, contents)
}

/// What a leaf source's keyed references do, both ways they arise, judged
/// the same way: the kernel takes the snapshot on a copy, and this crate's
/// must split the references between inline and items as the kernel's did.
fn snapshot_of_reflinks(tag: &str, reflinks: usize, already_keyed: bool) {
    let scratch = Scratch::new(tag);
    let (sub, extent, contents) = reflinked(&scratch, reflinks);
    let refs = reflinks as u64 + 1;

    let (inline_before, keyed_before) = inline_and_keyed(&dump_tree(&scratch.image, "2"), extent);
    assert_eq!(
        keyed_before > 0,
        already_keyed,
        "the premise: {inline_before} inline, {keyed_before} keyed"
    );

    // The kernel's own, on a copy: how many of its references are inline.
    let measured = scratch.copy("measured.img");
    let out = guest_kernel_write_ok(
        &measured,
        "the kernel's snapshot",
        r#"
cd "$MNT"
btrfs subvolume snapshot sub ksnap >/dev/null
sync
printf 'ksnap-id %s\n' "$(btrfs inspect-internal rootid ksnap)"
"#,
    );
    let measured = PathBuf::from(measured);
    let ksnap = printed_id(&out, "ksnap-id ");
    let (kernel_total, kernel_inline, kernel_keyed) =
        data_refs(&dump_tree(&measured, "2"), extent, ksnap);
    assert_eq!(
        kernel_inline + kernel_keyed,
        refs,
        "the kernel's references"
    );
    assert_eq!(kernel_total, 2 * refs, "the kernel's total");
    assert_btrfs_check_clean(&measured, "after the kernel's snapshot");

    let snap = {
        let mut fs = scratch.mount();
        let root = fs.root_inode().unwrap().ino;
        fs.create_snapshot(sub, root, b"snap", false)
            .expect("snapshot a source whose extent keeps keyed references")
    };
    assert_btrfs_check_clean(&scratch.image, "after the snapshot was taken");

    let (total, inline, keyed) = data_refs(&dump_tree(&scratch.image, "2"), extent, snap);
    assert_eq!(total, 2 * refs, "the record's total");
    assert_eq!(
        (inline, keyed),
        (kernel_inline, kernel_keyed),
        "the snapshot's references, inline and keyed, against the kernel's"
    );

    let (write_snap, snap_after) = scribble_reflinks("snap", &contents);
    let (write_sub, sub_after) = scribble_reflinks("sub", &contents);
    let script = format!(
        r#"cd "$MNT"
{before_sub}{before_snap}{write_snap}{write_sub}sync
{after_sub}{after_snap}"#,
        before_sub = sum("sub"),
        before_snap = sum("snap"),
        after_sub = sum("sub"),
        after_snap = sum("snap"),
    );
    let out = guest_kernel_write_ok(&scratch.image_str(), "reading and writing", &script);
    let printed: Vec<&str> = out.lines().filter(|l| l.starts_with("sum-")).collect();
    assert_eq!(printed.len(), 4, "the kernel's digests:\n{out}");
    let word = |line: &str| line.split_whitespace().nth(1).unwrap_or("").to_string();
    assert_eq!(word(printed[0]), listing(&contents), "sub as snapshotted");
    assert_eq!(word(printed[1]), listing(&contents), "snap as taken");
    assert_eq!(word(printed[2]), listing(&sub_after), "sub written");
    assert_eq!(word(printed[3]), listing(&snap_after), "snap written");
    assert_btrfs_check_clean(&scratch.image, "after the kernel wrote into both sides");

    let script = format!(
        r#"cd "$MNT"
btrfs subvolume delete snap >/dev/null
btrfs subvolume sync . >/dev/null && echo synced
{sub}"#,
        sub = sum("sub"),
    );
    let out = guest_kernel_write_ok(&scratch.image_str(), "deleting the snapshot", &script);
    assert!(
        out.lines().any(|l| l.trim() == "synced"),
        "the cleaner did not finish:\n{out}"
    );
    assert_kernel_read(&out, "sub", &sub_after);
    assert_btrfs_check_clean(&scratch.image, "after the kernel's cleaner dropped it");
}

/// The top level as the source, with a leaf holding both the snapshot's
/// entries and items naming an extent with keyed references: copying that
/// leaf gives the extent a shared reference naming the old block, which goes
/// in an item of its own, as the kernel files one beside other items.
#[test]
fn a_shared_leaf_naming_an_extent_with_keyed_references_is_copied() {
    let scratch = Scratch::new("keyed-shared");
    let data = pattern(64 << 10, 53);
    let source = scratch.dir.join("source");
    std::fs::write(&source, &data).expect("write the source bytes");
    let mut script = format!("cd \"$MNT\"\ncat {} > o\nsync\n", source.display());
    let mut contents = BTreeMap::new();
    contents.insert("o".to_string(), data.clone());
    for i in 1..=70 {
        script.push_str(&format!("cp --reflink=always o r{i:02}\n"));
        contents.insert(format!("r{i:02}"), data.clone());
    }
    script.push_str("sync\n");
    guest_kernel_write_ok(&scratch.image_str(), "the kernel's reflinks", &script);
    let dump = dump_tree(&scratch.image, "5");
    assert!(root_is_node(&dump), "the premise: the top level is tall");
    let extent = first_data_extent(&scratch.image, FS_TREE_OBJECTID);
    let (_, keyed) = inline_and_keyed(&dump_tree(&scratch.image, "2"), extent);
    assert!(keyed > 0, "the premise: the extent has keyed references");

    let snap = {
        let mut fs = scratch.mount();
        let root = fs.root_inode().unwrap().ino;
        fs.create_snapshot(FS_TREE_OBJECTID, root, b"snap", false)
            .expect("snapshot a top level whose leaf names keyed references")
    };
    assert_btrfs_check_clean(&scratch.image, "after the snapshot was taken");
    assert_children_name(&scratch.image, snap);
    let extents = dump_tree(&scratch.image, "2");
    assert!(
        items_of(&extents, extent)
            .iter()
            .any(|(key, _)| key.contains(" SHARED_DATA_REF ")),
        "the copied leaf's reference is not an item of its own"
    );

    let (write_snap, snap_after) = scribble_reflinks("snap", &contents);
    let script = format!(
        r#"cd "$MNT"
{top}{snap}{write_snap}sync
{after}"#,
        top = sum("."),
        snap = sum("snap"),
        after = sum("snap"),
    );
    let out = guest_kernel_write_ok(&scratch.image_str(), "reading and writing", &script);
    let printed: Vec<&str> = out.lines().filter(|l| l.starts_with("sum-")).collect();
    assert_eq!(printed.len(), 3, "the kernel's digests:\n{out}");
    let word = |line: &str| line.split_whitespace().nth(1).unwrap_or("").to_string();
    assert_eq!(word(printed[0]), listing(&contents), "the top level");
    assert_eq!(word(printed[1]), listing(&contents), "snap as taken");
    assert_eq!(word(printed[2]), listing(&snap_after), "snap written");
    assert_btrfs_check_clean(&scratch.image, "after the kernel wrote into the snapshot");

    let script = format!(
        r#"cd "$MNT"
btrfs subvolume delete snap >/dev/null
btrfs subvolume sync . >/dev/null && echo synced
{top}"#,
        top = sum("."),
    );
    let out = guest_kernel_write_ok(&scratch.image_str(), "deleting the snapshot", &script);
    assert!(
        out.lines().any(|l| l.trim() == "synced"),
        "the cleaner did not finish:\n{out}"
    );
    assert_kernel_read(&out, ".", &contents);
    assert_btrfs_check_clean(&scratch.image, "after the kernel's cleaner dropped it");
}

/// The kernel's script overwriting the start of each reflink in `tree`,
/// with what the tree should then hold.
fn scribble_reflinks(
    tree: &str,
    files: &BTreeMap<String, Vec<u8>>,
) -> (String, BTreeMap<String, Vec<u8>>) {
    let mut script = String::new();
    let mut after = files.clone();
    for (name, data) in after.iter_mut() {
        let tail = format!("{tree}-{name}");
        script.push_str(&format!(
            "printf '{tail}' | dd of={tree}/{name} bs=1 conv=notrunc status=none\n"
        ));
        data[..tail.len()].copy_from_slice(tail.as_bytes());
    }
    (script, after)
}

/// A data extent already keeping references as items of its own: the
/// kernel files every new one the same way.
#[test]
fn a_leaf_naming_an_extent_with_keyed_references_is_snapshotted() {
    snapshot_of_reflinks("keyed", 36, true);
}

/// A data extent whose record the snapshot's references would outgrow: as
/// many as stay under the limit go inline, the rest as items of their own.
#[test]
fn a_leaf_naming_an_extent_the_snapshot_fills_is_snapshotted() {
    snapshot_of_reflinks("fills", 20, false);
}

/// How many snapshots take a child block's record past what stays inline
/// on a 4 KiB node, where 22 tree references do.
const KEYED_SNAPSHOTS: usize = 30;

/// A child tree block whose record the snapshots fill: as many references as
/// stay under the limit go inline, and each after them is an item of its
/// own. The kernel takes the same snapshots of a copy, and this crate's must
/// file every reference where the kernel's went. On 4 KiB nodes: the 107
/// references a 16 KiB node's record holds take enough of the kernel's
/// commits to turn its free-space tree into a bitmap, which no write here
/// takes, and this crate's snapshots stop at the root tree's leaf unless
/// the kernel has grown it.
#[test]
fn a_child_block_gains_keyed_references_past_what_stays_inline() {
    let scratch = Scratch::small_nodes("keyed-block");
    // Enough trees that the root tree is taller than a leaf, in one commit.
    let mut script = String::from("cd \"$MNT\"\nbtrfs subvolume create sub >/dev/null\n");
    for i in 0..40 {
        script.push_str(&format!("btrfs subvolume create e{i:02} >/dev/null\n"));
    }
    script.push_str("sync\nprintf 'sub-id %s\\n' \"$(btrfs inspect-internal rootid sub)\"\n");
    let out = guest_kernel_write_ok(&scratch.image_str(), "the kernel's trees", &script);
    let sub = printed_id(&out, "sub-id ");
    let contents = files();
    fill(&mut scratch.mount().open_subvolume(sub).unwrap(), &contents);
    assert_btrfs_check_clean(&scratch.image, "after the kernel's fill");
    assert!(
        root_is_node(&dump_tree(&scratch.image, "1")),
        "the premise: the root tree is taller than a leaf"
    );
    let children = root_children(&scratch.image, sub);

    // The kernel's own filing of the same references, on a copy.
    let measured = scratch.copy("measured.img");
    let mut script = String::from("cd \"$MNT\"\n");
    for i in 0..KEYED_SNAPSHOTS {
        script.push_str(&format!(
            "btrfs subvolume snapshot sub k{i:03} >/dev/null\n"
        ));
    }
    script.push_str("sync\n");
    guest_kernel_write_ok(&measured, "the kernel's snapshots", &script);
    let measured = PathBuf::from(measured);
    assert_btrfs_check_clean(&measured, "after the kernel's snapshots");
    let kernel = dump_tree(&measured, "2");
    assert_eq!(
        root_children(&measured, sub),
        children,
        "the kernel's snapshots moved a child"
    );
    for &child in &children {
        let record = block_record(&kernel, child);
        assert!(
            !record.keyed.is_empty(),
            "the premise: the child at {child} has references of their own: {record:?}"
        );
        assert_eq!(record.total, record.listed(), "{record:?}");
    }

    let snaps: Vec<u64> = {
        let mut fs = scratch.mount();
        let root = fs.root_inode().unwrap().ino;
        (0..KEYED_SNAPSHOTS)
            .map(|i| {
                fs.create_snapshot(sub, root, format!("k{i:03}").as_bytes(), false)
                    .expect("snapshot a tree whose children keep keyed references")
            })
            .collect()
    };
    assert_btrfs_check_clean(&scratch.image, "after the snapshots were taken");

    let extents = dump_tree(&scratch.image, "2");
    for &child in &children {
        let want = block_record(&kernel, child);
        let got = block_record(&extents, child);
        assert_eq!(got.total, want.total, "the child at {child}");
        assert_eq!(got.inline, want.inline, "the child at {child}, inline");
        assert_eq!(got.keyed, want.keyed, "the child at {child}, as items");
    }
    let first = snaps[0];
    let last = snaps[KEYED_SNAPSHOTS - 1];
    assert_children_name(&scratch.image, first);
    assert_children_name(&scratch.image, last);

    let tall = format!("k{:03}", KEYED_SNAPSHOTS - 1);
    let (write_snap, snap_after) = scribble_small(&tall, &contents);
    let (write_sub, sub_after) = scribble_small("sub", &contents);
    let script = format!(
        r#"cd "$MNT"
{first_before}{write_snap}{write_sub}sync
{sub}{snap}{k}"#,
        first_before = sum("k000"),
        sub = sum("sub"),
        snap = sum(&tall),
        k = sum("k010"),
    );
    let out = guest_kernel_write_ok(&scratch.image_str(), "reading and writing", &script);
    let printed: Vec<&str> = out.lines().filter(|l| l.starts_with("sum-")).collect();
    assert_eq!(printed.len(), 4, "the kernel's digests:\n{out}");
    let word = |line: &str| line.split_whitespace().nth(1).unwrap_or("").to_string();
    assert_eq!(word(printed[0]), listing(&contents), "a snapshot as taken");
    assert_eq!(word(printed[1]), listing(&sub_after), "sub written");
    assert_eq!(
        word(printed[2]),
        listing(&snap_after),
        "the last snapshot written"
    );
    assert_eq!(word(printed[3]), listing(&contents), "another snapshot");
    assert_btrfs_check_clean(&scratch.image, "after the kernel wrote into both sides");

    let script = format!(
        r#"cd "$MNT"
btrfs subvolume delete {tall} >/dev/null
btrfs subvolume delete k000 >/dev/null
btrfs subvolume sync . >/dev/null && echo synced
{sub}{k}"#,
        sub = sum("sub"),
        k = sum("k010"),
    );
    let out = guest_kernel_write_ok(&scratch.image_str(), "deleting two snapshots", &script);
    assert!(
        out.lines().any(|l| l.trim() == "synced"),
        "the cleaner did not finish:\n{out}"
    );
    assert_kernel_read(&out, "sub", &sub_after);
    assert_kernel_read(&out, "k010", &contents);
    assert_btrfs_check_clean(&scratch.image, "after the kernel's cleaner dropped them");
}

/// The kernel's script appending to every file of `tree`, with what the
/// tree should then hold.
fn scribble_small(
    tree: &str,
    files: &BTreeMap<String, Vec<u8>>,
) -> (String, BTreeMap<String, Vec<u8>>) {
    let mut script = String::new();
    let mut after = files.clone();
    for (name, data) in after.iter_mut() {
        let tail = format!("{tree}-{name}");
        script.push_str(&format!("printf '{tail}' >> {tree}/{name}\n"));
        data.extend_from_slice(tail.as_bytes());
    }
    (script, after)
}
