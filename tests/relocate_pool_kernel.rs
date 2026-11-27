//! Block groups relocated within a pool, off one member, judged by the
//! kernel and btrfs-progs (#702).
//!
//! # What a relocation in a pool has to get right
//!
//! The new chunk's stripes go on a member other than the one being emptied,
//! and its device extents and `bytes_used` on that member; the old chunk's
//! device extents come off the member that held them, and its `bytes_used`
//! falls there, in the chunk tree's device item and in that member's own
//! superblock. Get the member wrong and `btrfs check` finds a device extent
//! no chunk names, or a device whose usage disagrees with its extents.
//!
//! # The oracles
//!
//! `mkfs.btrfs` makes a volume, single data and DUP metadata, and the kernel
//! writes its files; the manifest of them is taken then. This crate adds a
//! second device and relocates every data and metadata group off the first,
//! then, later, everything back off the second, which can then be removed.
//! After each step, in the harness VM with every member attached: `btrfs
//! check --readonly`, `btrfs inspect-internal dump-tree -t dev` for which
//! device holds each chunk's extents, `btrfs device usage` for what each
//! device has allocated, `btrfs filesystem show` against every member's own
//! superblock, and the kernel reading every file of the manifest. The kernel
//! also writes to the pool the first relocations left, and `btrfs check`
//! looks again.
//!
//! A member of a pool, single, DUP or mirrored, is also removed whole
//! (#816), and the members left are judged the same way, a mirrored pool by
//! `btrfs scrub` as well.

use fs_btrfs::block_group::BlockGroup;
use fs_btrfs::chunk::block_group::SYSTEM;
use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{
    guest_kernel_pool_read, guest_kernel_pool_update, guest_kernel_pool_write,
    guest_kernel_write_ok, oracle, sha256_hex, temp_path,
};
use fs_core::{BlockDevice, FileDevice};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MIB: u64 = 1 << 20;

/// A scratch directory of images, removed when it drops — including on a
/// panic.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        // In the repository's scratch directory: the harness VM sees this
        // repository and nothing else of the host.
        let dir = PathBuf::from(temp_path!(
            "btrfs-relocate-pool-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    /// An image of `size` bytes, every one of them zero.
    fn blank(&self, name: &str, size: u64) -> PathBuf {
        let image = self.0.join(name);
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(size))
            .unwrap();
        image
    }

    /// An image of `size` bytes holding a filesystem `mkfs.btrfs` made,
    /// single data and DUP metadata.
    fn formatted(&self, name: &str, size: u64) -> PathBuf {
        self.formatted_with_metadata(name, size, "dup")
    }

    /// [`Scratch::formatted`] with its metadata in `metadata`.
    fn formatted_with_metadata(&self, name: &str, size: u64, metadata: &str) -> PathBuf {
        let image = self.blank(name, size);
        let made = oracle("mkfs.btrfs")
            .args([
                "-q", "-f", "-d", "single", "-m", metadata, "-s", "4096", "-n", "16384",
            ])
            .arg(&image)
            .output();
        assert!(
            made.status.success(),
            "mkfs.btrfs: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        image
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn open_rw(path: &Path) -> Arc<dyn BlockDevice> {
    Arc::new(FileDevice::open_rw(path).expect("opening an image read-write"))
}

/// Every file under `$MNT` with its digest, as `sha256sum` prints it.
const MANIFEST: &str = "(cd \"$MNT\" && find . -type f -exec sha256sum {} + | sort)\n";

/// `path -> sha256` from `sha256sum` lines, the path as this crate looks
/// it up.
fn manifest(output: &str) -> BTreeMap<String, String> {
    output
        .lines()
        .filter_map(|line| line.split_once("  "))
        .filter(|(sum, _)| sum.len() == 64 && sum.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(|(sum, name)| (name.trim_start_matches('.').to_string(), sum.to_string()))
        .collect()
}

/// The non-system groups with a stripe on device `devid`, data first: the
/// data groups' moves write tree blocks, which the metadata groups' moves
/// then take with them.
fn on_device(fs: &Filesystem, devid: u64) -> Vec<BlockGroup> {
    let mut groups: Vec<BlockGroup> = fs
        .block_groups()
        .unwrap()
        .into_iter()
        .filter(|g| g.flags & SYSTEM == 0 && devices_of(fs, g.start).contains(&devid))
        .collect();
    groups.sort_by_key(|g| g.holds_metadata());
    groups
}

/// The devices the chunk at `start` has a stripe on.
fn devices_of(fs: &Filesystem, start: u64) -> BTreeSet<u64> {
    fs.chunk_map()
        .chunk_for(start)
        .expect("a chunk")
        .stripes
        .iter()
        .map(|s| s.devid)
        .collect()
}

/// Relocate every data and metadata group off device `from`, and return
/// where each new one is.
fn empty_device(fs: &mut Filesystem, from: u64) -> Vec<u64> {
    let mut moved = Vec::new();
    for old in on_device(fs, from) {
        let new = fs
            .relocate_block_group_off(old.start, from)
            .unwrap_or_else(|e| {
                panic!(
                    "relocating the group at {} off device {from}: {e}",
                    old.start
                )
            });
        let replacement = fs
            .block_groups()
            .unwrap()
            .into_iter()
            .find(|g| g.start == new)
            .expect("the new group");
        assert_eq!(
            (replacement.flags, replacement.length),
            (old.flags, old.length),
            "the group at {} moved into a group of another kind or size",
            old.start
        );
        assert!(
            !devices_of(fs, new).contains(&from),
            "the group relocated off device {from} has a stripe on it"
        );
        moved.push(new);
    }
    assert!(
        !moved.is_empty(),
        "device {from} held no data or metadata group"
    );
    assert!(
        on_device(fs, from).is_empty(),
        "groups are left on device {from}"
    );
    moved
}

/// What the guest is asked about the pool.
const SHOW: &str = "btrfs filesystem show --raw \"$MNT\" | \
     awk '$1 == \"devid\" {print \"devid\\t\" $2 \"\\t\" $4 \"\\t\" $6}'\n\
     btrfs device usage -b \"$MNT\" | sed 's/^/usage\\t/'\n\
     btrfs inspect-internal dump-tree -t dev \"$(findmnt -n -o SOURCE \"$MNT\")\" | \
     awk '/DEV_EXTENT/ {gsub(/[()]/, \"\"); dev = $4} /chunk_offset/ \
     {for (i = 1; i < NF; i++) if ($i == \"chunk_offset\") print \"extent\\t\" dev \"\\t\" $(i + 1)}'\n";

/// `devid -> used` from the `devid` lines.
fn shown_used(text: &str) -> BTreeMap<u64, u64> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            (f.len() == 4 && f[0] == "devid").then(|| {
                let n = |s: &str| s.parse::<u64>().unwrap_or_else(|_| panic!("{line}"));
                (n(f[1]), n(f[3]))
            })
        })
        .collect()
}

/// `chunk logical -> devids` from the `extent` lines `dump-tree -t dev`
/// gave.
fn extents_by_chunk(text: &str) -> BTreeMap<u64, BTreeSet<u64>> {
    let mut out: BTreeMap<u64, BTreeSet<u64>> = BTreeMap::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() == 3 && f[0] == "extent" {
            let n = |s: &str| s.parse::<u64>().unwrap_or_else(|_| panic!("{line}"));
            out.entry(n(f[2])).or_default().insert(n(f[1]));
        }
    }
    out
}

/// `devid -> the kinds it has allocated` ("Data", "Metadata", "System"),
/// from `btrfs device usage`.
fn allocated_kinds(text: &str) -> BTreeMap<u64, BTreeSet<String>> {
    let mut out: BTreeMap<u64, BTreeSet<String>> = BTreeMap::new();
    let mut device = None;
    for line in text.lines().filter_map(|l| l.strip_prefix("usage\t")) {
        if let Some((_, id)) = line.split_once(", ID: ") {
            let id = id
                .trim()
                .parse::<u64>()
                .unwrap_or_else(|_| panic!("{line}"));
            out.entry(id).or_default();
            device = Some(id);
        } else if let (Some(id), Some((kind, _))) = (device, line.trim().split_once(',')) {
            if ["Data", "Metadata", "System"].contains(&kind) {
                out.entry(id).or_default().insert(kind.to_string());
            }
        }
    }
    out
}

/// Each member's own superblock's `devid -> bytes_used`.
fn recorded_used(members: &[&PathBuf]) -> BTreeMap<u64, u64> {
    members
        .iter()
        .map(|path| {
            let dev = FileDevice::open(path).expect("open a member");
            let (sb, copy) = fs_btrfs::superblock::read_superblock(&dev).expect("its superblock");
            assert_eq!(copy, 0, "{}: the primary is not the newest", path.display());
            (sb.dev_item.devid, sb.dev_item.bytes_used)
        })
        .collect()
}

/// Every oracle over `members`, after the groups at `moved` were relocated
/// onto device `onto`: check clean, each moved chunk's device extents on
/// `onto` alone, no data or metadata allocated on `off`, every member's
/// recorded usage the kernel's, and every file of `files`.
fn the_kernel_agrees(
    members: &[&PathBuf],
    moved: &[u64],
    onto: u64,
    off: u64,
    files: &BTreeMap<String, String>,
) {
    let images: Vec<String> = members
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let images: Vec<&str> = images.iter().map(String::as_str).collect();
    let read = guest_kernel_pool_read(&images, &format!("{SHOW}{MANIFEST}"));
    assert_eq!(
        read.check_status, 0,
        "btrfs check --readonly does not find the relocated pool clean:\n{}",
        read.check
    );
    let out = String::from_utf8_lossy(&read.out.stdout).into_owned();
    assert!(
        read.out.status.success(),
        "the kernel could not mount and read the relocated pool:\n{out}{}",
        String::from_utf8_lossy(&read.out.stderr)
    );
    let extents = extents_by_chunk(&out);
    for at in moved {
        assert_eq!(
            extents.get(at),
            Some(&BTreeSet::from([onto])),
            "dump-tree -t dev: the chunk at {at} is not on device {onto} alone:\n{out}"
        );
    }
    let kinds = allocated_kinds(&out);
    if let Some(left) = kinds.get(&off) {
        assert!(
            !left.contains("Data") && !left.contains("Metadata"),
            "btrfs device usage: device {off} still has {left:?} allocated:\n{out}"
        );
    }
    assert!(
        kinds
            .get(&onto)
            .is_some_and(|k| k.contains("Data") && k.contains("Metadata")),
        "btrfs device usage: device {onto} has no data and metadata allocated:\n{out}"
    );
    assert_eq!(
        shown_used(&out),
        recorded_used(members),
        "btrfs filesystem show and the members' superblocks disagree on usage:\n{out}"
    );
    assert_eq!(
        &manifest(&out),
        files,
        "the kernel reads the files differently after the relocation"
    );
}

#[test]
fn groups_relocated_off_each_member_in_turn_leave_pools_the_kernel_reads() {
    let scratch = Scratch::new("turns");
    let a = scratch.formatted("a.img", 256 * MIB);
    let b = scratch.blank("b.img", 256 * MIB);

    // The kernel writes the files, and the manifest is taken while it has
    // them mounted: nothing here has touched the volume yet.
    let wrote = guest_kernel_write_ok(
        &a.to_string_lossy(),
        "the kernel writes the files the relocations must keep",
        &format!(
            "mkdir \"$MNT/files\"\n\
             for i in $(seq 1 160); do head -c 8192 /dev/urandom > \"$MNT/files/f$i\"; done\n\
             head -c 3000000 /dev/urandom > \"$MNT/large.bin\"\n\
             sync\n{MANIFEST}"
        ),
    );
    let files = manifest(&wrote);
    assert_eq!(files.len(), 161, "the kernel's manifest:\n{wrote}");

    // A second device, and every data and metadata group moved onto it.
    let mut fs = Filesystem::mount_rw(open_rw(&a)).expect("the volume mounts read-write");
    assert_eq!(fs.add_device(open_rw(&b)).expect("adding b"), 2);
    let moved = empty_device(&mut fs, 1);
    for (path, digest) in &files {
        let data = fs
            .read_path(path)
            .expect("a manifest file, through this driver");
        assert_eq!(
            &sha256_hex(&data),
            digest,
            "{path} reads back differently through this driver after the relocation"
        );
    }
    drop(fs);
    the_kernel_agrees(&[&a, &b], &moved, 2, 1, &files);

    // The kernel writes to the pool the relocations left.
    let images = [
        a.to_string_lossy().into_owned(),
        b.to_string_lossy().into_owned(),
    ];
    let images: Vec<&str> = images.iter().map(String::as_str).collect();
    let written = guest_kernel_pool_write(
        &images,
        "dd if=/dev/urandom of=\"$MNT/after\" bs=1M count=32 status=none\nsync\n",
    );
    assert!(
        written.out.status.success(),
        "the kernel could not write to the relocated pool:\n{}",
        String::from_utf8_lossy(&written.out.stderr)
    );
    assert_eq!(
        written.check_status, 0,
        "btrfs check --readonly does not find the pool clean after the kernel wrote to it:\n{}",
        written.check
    );

    // Everything back off the second device, which then holds no chunk and
    // can be removed.
    let mut fs = Filesystem::mount_pool_rw(vec![open_rw(&a), open_rw(&b)])
        .expect("the pool mounts read-write");
    let moved = empty_device(&mut fs, 2);
    drop(fs);
    the_kernel_agrees(&[&a, &b], &moved, 1, 2, &files);
    let mut fs = Filesystem::mount_pool_rw(vec![open_rw(&a), open_rw(&b)])
        .expect("the pool mounts read-write");
    fs.remove_device(2)
        .unwrap_or_else(|e| panic!("removing the emptied device: {e}"));
    drop(fs);
    let read = guest_kernel_pool_read(&[&a.to_string_lossy()], MANIFEST);
    assert_eq!(
        read.check_status, 0,
        "btrfs check --readonly does not find the volume clean after the removal:\n{}",
        read.check
    );
    assert_eq!(
        manifest(&String::from_utf8_lossy(&read.out.stdout)),
        files,
        "the kernel reads the files differently after the removal"
    );
}

/// The `(flags, length)` of every block group, in order.
fn group_shapes(fs: &Filesystem) -> Vec<(u64, u64)> {
    let mut shapes: Vec<(u64, u64)> = fs
        .block_groups()
        .unwrap()
        .into_iter()
        .map(|g| (g.flags, g.length))
        .collect();
    shapes.sort_unstable();
    shapes
}

/// A two-member pool whose data and metadata are on device 2, which the
/// kernel has since written to, has that device removed in one call: every
/// group moves to device 1 in its own profile, and device 1 alone is a
/// volume `btrfs check` finds clean and the kernel reads whole (#816,
/// POOL-01).
fn removal_evacuates_a_member(metadata: &str) {
    let scratch = Scratch::new(&format!("evacuate-{metadata}"));
    let a = scratch.formatted_with_metadata("a.img", 256 * MIB, metadata);
    let b = scratch.blank("b.img", 256 * MIB);
    guest_kernel_write_ok(
        &a.to_string_lossy(),
        "the kernel writes the first files",
        "mkdir \"$MNT/files\"\n\
         for i in $(seq 1 160); do head -c 8192 /dev/urandom > \"$MNT/files/f$i\"; done\n\
         head -c 3000000 /dev/urandom > \"$MNT/large.bin\"\n\
         sync\n",
    );

    // Every data and metadata group onto a second device, and then the
    // kernel writes to the pool, kept for the removal, and takes the
    // manifest.
    let mut fs = Filesystem::mount_rw(open_rw(&a)).expect("the volume mounts read-write");
    assert_eq!(fs.add_device(open_rw(&b)).expect("adding b"), 2);
    empty_device(&mut fs, 1);
    drop(fs);
    let images = [
        a.to_string_lossy().into_owned(),
        b.to_string_lossy().into_owned(),
    ];
    let images: Vec<&str> = images.iter().map(String::as_str).collect();
    let written = guest_kernel_pool_update(
        &images,
        &format!("head -c 2000000 /dev/urandom > \"$MNT/pool.bin\"\nsync\n{MANIFEST}"),
    );
    let out = String::from_utf8_lossy(&written.out.stdout).into_owned();
    assert!(
        written.out.status.success(),
        "the kernel could not write to the pool:\n{out}{}",
        String::from_utf8_lossy(&written.out.stderr)
    );
    assert_eq!(written.check_status, 0, "{}", written.check);
    let files = manifest(&out);
    assert_eq!(files.len(), 162, "the kernel's manifest:\n{out}");

    let mut fs = Filesystem::mount_pool_rw(vec![open_rw(&a), open_rw(&b)])
        .expect("the pool mounts read-write");
    let held = on_device(&fs, 2);
    assert!(
        held.iter().any(|g| g.holds_metadata()) && held.iter().any(|g| !g.holds_metadata()),
        "device 2 does not hold both data and metadata"
    );
    let shapes = group_shapes(&fs);
    fs.remove_device(2)
        .unwrap_or_else(|e| panic!("removing a device that holds chunks: {e}"));
    assert_eq!(fs.superblock().num_devices, 1);
    assert_eq!(
        group_shapes(&fs),
        shapes,
        "the removal changed a group's profile, kind or size"
    );
    for (path, digest) in &files {
        let data = fs
            .read_path(path)
            .expect("a manifest file, through this driver");
        assert_eq!(
            &sha256_hex(&data),
            digest,
            "{path} reads back differently through this driver after the removal"
        );
    }
    drop(fs);
    assert!(
        fs_btrfs::superblock::read_superblock(&FileDevice::open(&b).unwrap()).is_err(),
        "the removed device still carries a superblock"
    );

    let read = guest_kernel_pool_read(&[&a.to_string_lossy()], &format!("{SHOW}{MANIFEST}"));
    assert_eq!(
        read.check_status, 0,
        "btrfs check --readonly does not find the volume clean after the removal:\n{}",
        read.check
    );
    let out = String::from_utf8_lossy(&read.out.stdout).into_owned();
    assert!(
        read.out.status.success(),
        "the kernel could not mount and read the volume the removal left:\n{out}{}",
        String::from_utf8_lossy(&read.out.stderr)
    );
    assert_eq!(
        shown_used(&out),
        recorded_used(&[&a]),
        "btrfs filesystem show does not list device 1 alone with its usage:\n{out}"
    );
    let extents = extents_by_chunk(&out);
    assert!(
        !extents.is_empty() && extents.values().all(|d| *d == BTreeSet::from([1])),
        "dump-tree -t dev: a device extent is not on device 1:\n{out}"
    );
    assert_eq!(
        manifest(&out),
        files,
        "the kernel reads the files differently after the removal"
    );
}

#[test]
fn removing_a_member_with_data_and_single_metadata_evacuates_it() {
    removal_evacuates_a_member("single");
}

#[test]
fn removing_a_member_with_data_and_dup_metadata_evacuates_it() {
    removal_evacuates_a_member("dup");
}

/// A pool in a mirrored `profile` of `copies` copies, with one member more
/// than that, has a member holding its data and metadata removed: every
/// group moves, in its own profile, onto `copies` members of those left,
/// and they are a pool `btrfs check` finds clean, `btrfs scrub` finds no
/// error in and the kernel reads whole (#816, POOL-03).
fn removal_evacuates_a_mirrored_member(profile: &str, copies: usize) {
    let scratch = Scratch::new(&format!("mirrored-{profile}"));
    let members: Vec<PathBuf> = (1..=copies + 1)
        .map(|i| scratch.blank(&format!("{i}.img"), 512 * MIB))
        .collect();
    let paths: Vec<String> = members
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
    // Made over one member fewer than the pool will have, so the system
    // group has no stripe on the member added next, which is removed later.
    let mkfs = members[..copies].iter().fold(
        oracle("mkfs.btrfs").args([
            "-q", "-f", "-d", profile, "-m", profile, "-s", "4096", "-n", "16384",
        ]),
        |mkfs, member| mkfs.arg(member),
    );
    let made = mkfs.output();
    assert!(
        made.status.success(),
        "mkfs.btrfs: {}",
        String::from_utf8_lossy(&made.stderr)
    );
    let wrote = guest_kernel_pool_update(
        &paths[..copies],
        "mkdir \"$MNT/files\"\n\
         for i in $(seq 1 160); do head -c 8192 /dev/urandom > \"$MNT/files/f$i\"; done\n\
         head -c 3000000 /dev/urandom > \"$MNT/large.bin\"\n\
         sync\n",
    );
    assert!(
        wrote.out.status.success(),
        "the kernel could not write the first files:\n{}",
        String::from_utf8_lossy(&wrote.out.stderr)
    );
    assert_eq!(wrote.check_status, 0, "{}", wrote.check);

    // The last member joins, every data and metadata group moves off the
    // first and so onto it, and the kernel writes to that pool.
    let added = copies as u64 + 1;
    let open_all = || members.iter().map(|p| open_rw(p)).collect::<Vec<_>>();
    let mut fs = Filesystem::mount_pool_rw(open_all()[..copies].to_vec())
        .expect("the pool mounts read-write");
    assert_eq!(
        fs.add_device(open_rw(&members[copies]))
            .expect("adding the last member"),
        added
    );
    empty_device(&mut fs, 1);
    drop(fs);
    let written = guest_kernel_pool_update(
        &paths,
        &format!("head -c 2000000 /dev/urandom > \"$MNT/pool.bin\"\nsync\n{MANIFEST}"),
    );
    let out = String::from_utf8_lossy(&written.out.stdout).into_owned();
    assert!(
        written.out.status.success(),
        "the kernel could not write to the pool:\n{out}{}",
        String::from_utf8_lossy(&written.out.stderr)
    );
    assert_eq!(written.check_status, 0, "{}", written.check);
    let files = manifest(&out);
    assert_eq!(files.len(), 162, "the kernel's manifest:\n{out}");

    let mut fs = Filesystem::mount_pool_rw(open_all()).expect("the pool mounts read-write");
    let held = on_device(&fs, added);
    assert!(
        held.iter().any(|g| g.holds_metadata()) && held.iter().any(|g| !g.holds_metadata()),
        "device {added} does not hold both data and metadata"
    );
    let shapes = group_shapes(&fs);
    fs.remove_device(added)
        .unwrap_or_else(|e| panic!("removing a member of a {profile} pool: {e}"));
    assert_eq!(fs.superblock().num_devices, copies as u64);
    assert_eq!(
        group_shapes(&fs),
        shapes,
        "the removal changed a group's profile, kind or size"
    );
    let survivors: BTreeSet<u64> = (1..=copies as u64).collect();
    for group in fs.block_groups().unwrap() {
        assert_eq!(
            devices_of(&fs, group.start),
            survivors,
            "the group at {} does not have a copy on each of {copies} survivors",
            group.start
        );
    }
    for (path, digest) in &files {
        let data = fs
            .read_path(path)
            .expect("a manifest file, through this driver");
        assert_eq!(
            &sha256_hex(&data),
            digest,
            "{path} reads back differently through this driver after the removal"
        );
    }
    drop(fs);
    assert!(
        fs_btrfs::superblock::read_superblock(&FileDevice::open(&members[copies]).unwrap())
            .is_err(),
        "the removed device still carries a superblock"
    );

    let read = guest_kernel_pool_read(
        &paths[..copies],
        &format!("{SHOW}btrfs scrub start -B -r \"$MNT\"\n{MANIFEST}"),
    );
    assert_eq!(
        read.check_status, 0,
        "btrfs check --readonly does not find the pool clean after the removal:\n{}",
        read.check
    );
    let out = String::from_utf8_lossy(&read.out.stdout).into_owned();
    assert!(
        read.out.status.success(),
        "the kernel could not mount, scrub and read the pool the removal left:\n{out}{}",
        String::from_utf8_lossy(&read.out.stderr)
    );
    assert!(
        out.contains("no errors found"),
        "btrfs scrub does not report a clean pool:\n{out}"
    );
    let left: Vec<&PathBuf> = members[..copies].iter().collect();
    assert_eq!(
        shown_used(&out),
        recorded_used(&left),
        "btrfs filesystem show and the survivors' superblocks disagree:\n{out}"
    );
    let extents = extents_by_chunk(&out);
    assert!(
        !extents.is_empty() && extents.values().all(|d| *d == survivors),
        "dump-tree -t dev: a chunk is not on each survivor:\n{out}"
    );
    assert_eq!(
        manifest(&out),
        files,
        "the kernel reads the files differently after the removal"
    );
}

#[test]
fn removing_a_member_of_a_raid1_pool_evacuates_it() {
    removal_evacuates_a_mirrored_member("raid1", 2);
}

#[test]
fn removing_a_member_of_a_raid1c3_pool_evacuates_it() {
    removal_evacuates_a_mirrored_member("raid1c3", 3);
}

#[test]
fn removing_a_member_of_a_raid1c4_pool_evacuates_it() {
    removal_evacuates_a_mirrored_member("raid1c4", 4);
}

/// The system group cannot be moved yet, so the member holding it is refused
/// before any other group on it is touched.
#[test]
fn removing_the_member_with_the_system_group_is_refused_and_writes_nothing() {
    let scratch = Scratch::new("system");
    let a = scratch.blank("a.img", 256 * MIB);
    let b = scratch.blank("b.img", 256 * MIB);
    fs_btrfs::mkfs::format(&*open_rw(&a), &fs_btrfs::mkfs::Options::default())
        .expect("this crate's mkfs");
    let mut fs = Filesystem::mount_rw(open_rw(&a)).expect("mounts read-write");
    assert_eq!(fs.add_device(open_rw(&b)).expect("adding b"), 2);
    let before: Vec<_> = [&a, &b]
        .iter()
        .map(|p| fs_btrfs_test_support::snapshot_image(p).unwrap())
        .collect();
    let msg = fs
        .remove_device(1)
        .expect_err("the device holding the system group was removed")
        .to_string();
    assert!(msg.contains("system block group"), "{msg}");
    drop(fs);
    for (path, bytes) in [&a, &b].iter().zip(&before) {
        fs_btrfs_test_support::assert_image_unchanged(
            path,
            bytes,
            &format!("a refusal wrote to {}", path.display()),
        );
    }
}

/// Every refusal leaves every image exactly as it was.
#[test]
fn a_relocation_off_a_device_that_cannot_be_made_is_refused_and_writes_nothing() {
    let scratch = Scratch::new("refused");
    let volume = scratch.blank("a.img", 256 * MIB);
    fs_btrfs::mkfs::format(&*open_rw(&volume), &fs_btrfs::mkfs::Options::default())
        .expect("this crate's mkfs");
    let data = |fs: &Filesystem| {
        fs.block_groups()
            .unwrap()
            .into_iter()
            .find(|g| !g.holds_metadata() && g.flags & SYSTEM == 0)
            .expect("a data group")
    };

    // One device: there is no pool to move a group within.
    let mut fs = Filesystem::mount_rw(open_rw(&volume)).expect("mounts read-write");
    let group = data(&fs);
    let before = fs_btrfs_test_support::snapshot_image(&volume).unwrap();
    let msg = fs
        .relocate_block_group_off(group.start, 1)
        .expect_err("a group was moved off a volume's only device")
        .to_string();
    assert!(msg.contains("not a member"), "{msg}");
    fs_btrfs_test_support::assert_image_unchanged(
        &volume,
        &before,
        "a refusal wrote to the volume",
    );

    // A second device with no room for the group.
    let small = scratch.blank("small.img", 2 * MIB);
    assert_eq!(
        fs.add_device(open_rw(&small))
            .expect("adding a small device"),
        2
    );
    let images = [&volume, &small];
    let before: Vec<_> = images
        .iter()
        .map(|p| fs_btrfs_test_support::snapshot_image(p).unwrap())
        .collect();
    let msg = fs
        .relocate_block_group_off(group.start, 1)
        .expect_err("a group was moved onto a device with no room for it")
        .to_string();
    assert!(msg.contains("has no room"), "{msg}");
    let msg = fs
        .relocate_block_group_off(group.start, 9)
        .expect_err("a group was moved off a device that is not in the pool")
        .to_string();
    assert!(msg.contains("not a member"), "{msg}");
    drop(fs);
    for (path, bytes) in images.iter().zip(&before) {
        fs_btrfs_test_support::assert_image_unchanged(
            path,
            bytes,
            &format!("a refusal wrote to {}", path.display()),
        );
    }
}
