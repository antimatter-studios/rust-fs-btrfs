//! A repairing scrub leaves a volume the kernel's scrub and `btrfs check`
//! both find clean (#302).
//!
//! The fixture is `scrub/btrfs-scrub.img`: a DUP volume the kernel
//! populated, then three copies damaged on purpose — the second copy of a
//! data sector, the first copy of the next, and the second copy of the fs
//! tree's root block. Every one has a good twin.
//!
//! `scrub_repair` writes the good twin's bytes over each bad copy, with
//! no transaction. Three oracles judge the result: this crate's own scrub
//! must find nothing; the kernel's read-only scrub (`btrfs scrub start -B
//! -R -r`) must count no read, checksum or verify error; and `btrfs check
//! --readonly` must find the volume clean. A repair that wrote the wrong
//! copy, the wrong offset, or a block with a stale checksum fails at
//! least one of them.
//!
//! # Pools, and RAID5/6 (#350)
//!
//! The same, on pools mounted with `mount_pool_rw`: the RAID1, RAID5 and
//! RAID6 pools the kernel made and filled (`btrfs-pool-*`,
//! `btrfs-raid5-*`, `btrfs-raid6-*`), each copied and damaged here --
//! two data sectors of `big.bin` and the fs tree's root block, on RAID1
//! one copy on each member. On RAID5/6 a block has one copy, so the bytes
//! to repair it with come from a rebuild from parity. Before the repair
//! the kernel's read-only scrub must count the damage, so the damage is
//! somewhere the kernel looks; after it, every member must be byte for
//! byte the member the kernel made, `btrfs check --readonly` must find the
//! pool clean, the kernel's scrub must count nothing, and the kernel must
//! read every file back as the fixture's manifest says it wrote it.
//!
//! And the parity fixtures `scrub/btrfs-scrub-raid5-*` and `-raid6-*`,
//! whose P (and Q) the guest damaged where the chunk tree as btrfs-progs
//! prints it puts them: a repair computes each element again from its
//! data, and it must hash to what the kernel's own repairing scrub wrote
//! there.

use fs_btrfs::chunk::DiskKey;
use fs_btrfs::fs::Filesystem;
use fs_btrfs::scrub::ScrubTarget;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, fixture, guest_kernel_pool_read, guest_kernel_read_ok, le64,
    sha256_hex, temp_path,
};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A copy of the damaged fixture, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        // In the repository's scratch directory: the harness VM sees this
        // repository and nothing else of the host.
        let dir = PathBuf::from(temp_path!(
            "btrfs-scrub-repair-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(fixture("scrub/btrfs-scrub.img"), dir.join("fs.img"))
            .expect("copying the scrub fixture");
        Self(dir)
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

/// A counter from `btrfs scrub start -R` output.
fn counter(report: &str, name: &str) -> u64 {
    report
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix(&format!("{name}: ")))
        .unwrap_or_else(|| panic!("the kernel's scrub printed no {name}:\n{report}"))
        .trim()
        .parse()
        .expect("a count")
}

#[test]
fn every_bad_copy_is_repaired_and_the_kernel_and_checker_agree() {
    let scratch = Scratch::new("all");
    let image = scratch.image();

    let dev = Arc::new(FileDevice::open_rw(&image).expect("open read-write"));
    let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount read-write");
    let report = fs.scrub_repair().expect("scrub repair");
    assert_eq!(
        report.scrub.errors.len(),
        3,
        "the fixture damages three copies: {:#?}",
        report.scrub.errors
    );
    assert!(
        report.unrepairable.is_empty(),
        "every damaged copy has a good twin: {:#?}",
        report.unrepairable
    );
    let found: Vec<(ScrubTarget, u64, usize)> = report
        .scrub
        .errors
        .iter()
        .map(|e| (e.what, e.logical, e.mirror))
        .collect();
    assert_eq!(report.repaired, found, "each bad copy repaired once");
    drop(fs);

    // Our own scrub, on a fresh mount, finds nothing.
    let dev = Arc::new(FileDevice::open(&image).expect("open"));
    let again = Filesystem::mount(dev)
        .expect("mount the repaired volume")
        .scrub()
        .expect("scrub after the repair");
    assert!(again.errors.is_empty(), "{:#?}", again.errors);

    assert_btrfs_check_clean(&image, "after scrub repair");

    let kernel = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "kernel scrub after the repair",
        "btrfs scrub start -B -R -r \"$MNT\"",
    );
    for name in ["read_errors", "csum_errors", "verify_errors"] {
        assert_eq!(
            counter(&kernel, name),
            0,
            "the kernel's scrub still finds {name} after the repair:\n{kernel}"
        );
    }
}

/// A read-only mount repairs nothing and says so.
#[test]
fn a_read_only_mount_is_refused() {
    let dev = Arc::new(FileDevice::open(fixture("scrub/btrfs-scrub.img")).expect("open"));
    let fs = Filesystem::mount(dev).expect("mount");
    assert!(matches!(
        fs.scrub_repair(),
        Err(fs_btrfs::error::Error::ReadOnly)
    ));
}

/// `EXTENT_DATA`, the key type of a file extent item.
const EXTENT_DATA: u8 = 108;

/// `BTRFS_FILE_EXTENT_REG`: an extent of its own on disk.
const FILE_EXTENT_REG: u8 = 1;

/// Where `dev_item.devid` lies on a member: the primary superblock at
/// 64 KiB, its device item at 0xc9.
const SUPER_DEVID: u64 = 65_536 + 0xc9;

/// The fs tree's objectid.
const FS_TREE: u64 = 5;

/// Copies of a pool's members, removed when dropped -- including on a
/// panic.
struct PoolScratch {
    dir: PathBuf,
    /// The fixture each member was copied from, in the same order.
    sources: Vec<PathBuf>,
    members: Vec<PathBuf>,
}

impl PoolScratch {
    fn new(tag: &str, sources: Vec<PathBuf>) -> Self {
        // In the repository's scratch directory: the harness VM sees this
        // repository and nothing else of the host.
        let dir = PathBuf::from(temp_path!(
            "btrfs-scrub-repair-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let members = sources
            .iter()
            .enumerate()
            .map(|(i, src)| {
                let to = dir.join(format!("{}.img", i + 1));
                std::fs::copy(src, &to)
                    .unwrap_or_else(|e| panic!("copying {}: {e}", src.display()));
                to
            })
            .collect();
        Self {
            dir,
            sources,
            members,
        }
    }

    /// Each member by the devid its own superblock names.
    fn by_devid(&self) -> BTreeMap<u64, PathBuf> {
        self.members
            .iter()
            .map(|p| {
                let mut b = [0u8; 8];
                FileDevice::open(p)
                    .expect("open a member")
                    .read_at(SUPER_DEVID, &mut b)
                    .expect("read a member's superblock");
                (u64::from_le_bytes(b), p.clone())
            })
            .collect()
    }

    fn readable(&self) -> Filesystem {
        let devices = self
            .members
            .iter()
            .map(|p| Arc::new(FileDevice::open(p).expect("open a member")) as Arc<dyn BlockRead>)
            .collect();
        Filesystem::mount_pool(devices).expect("mount the pool read-only")
    }

    fn writable(&self) -> Filesystem {
        let devices = self
            .members
            .iter()
            .map(|p| {
                Arc::new(FileDevice::open_rw(p).expect("open a member read-write"))
                    as Arc<dyn BlockDevice>
            })
            .collect();
        Filesystem::mount_pool_rw(devices).expect("mount the pool read-write")
    }

    fn images(&self) -> Vec<String> {
        self.members
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    }
}

impl Drop for PoolScratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The members of a fixture pool, `<name>-<suffix>.img` for each suffix.
fn sources(name: &str, suffixes: &[&str]) -> Vec<PathBuf> {
    suffixes
        .iter()
        .map(|s| fixture(&format!("{name}-{s}.img")))
        .collect()
}

/// The disk address and length of the extent holding the first byte of
/// `path`.
fn first_extent(fs: &Filesystem, path: &str) -> (u64, u64) {
    let ino = fs.lookup_path(path).expect("the file").ino;
    let root = fs.tree_root_public(FS_TREE).expect("the fs tree");
    let mut found = None;
    fs.for_each_item_in(root, &mut |k: &DiskKey, item: &[u8]| {
        // generation, ram_bytes, compression, encryption, other_encoding,
        // type at 20, then disk_bytenr and disk_num_bytes.
        if found.is_none()
            && k.objectid == ino
            && k.key_type == EXTENT_DATA
            && k.offset == 0
            && item.len() >= 37
            && item[20] == FILE_EXTENT_REG
        {
            found = Some((le64(item, 21), le64(item, 29)));
        }
    })
    .expect("walking the fs tree");
    found.unwrap_or_else(|| panic!("{path} has no regular extent at offset 0"))
}

/// Invert 64 bytes, 512 bytes into the element at `physical` on `member`:
/// past a tree block's checksum field, so the block no longer verifies
/// and is still one; as the fixtures' own damage is placed.
fn damage(member: &Path, physical: u64) {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(member)
        .expect("open a member to damage it");
    let mut b = [0u8; 64];
    f.seek(SeekFrom::Start(physical + 512)).unwrap();
    f.read_exact(&mut b).expect("read the bytes to damage");
    for x in &mut b {
        *x ^= 0xff;
    }
    f.seek(SeekFrom::Start(physical + 512)).unwrap();
    f.write_all(&b).expect("damage a member");
}

/// Whether two files hold the same bytes.
fn same_bytes(a: &Path, b: &Path) -> bool {
    let (mut a, mut b) = (
        std::fs::File::open(a).expect("open"),
        std::fs::File::open(b).expect("open"),
    );
    if a.metadata().unwrap().len() != b.metadata().unwrap().len() {
        return false;
    }
    let (mut x, mut y) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    loop {
        let n = a.read(&mut x).expect("read");
        if n == 0 {
            return true;
        }
        b.read_exact(&mut y[..n]).expect("read");
        if x[..n] != y[..n] {
            return false;
        }
    }
}

/// Every file a fixture's manifest names, `path -> (size, sha256)`.
fn files(manifest: &str) -> BTreeMap<String, (String, String)> {
    let text = std::fs::read_to_string(fixture(manifest)).expect("a pool manifest");
    tab_rows(&text)
}

/// The `path\tsize\tsha256` rows of `text`, directories left out.
fn tab_rows(text: &str) -> BTreeMap<String, (String, String)> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            (f.len() == 3 && f[1] != "dir")
                .then(|| (f[0].to_string(), (f[1].to_string(), f[2].to_string())))
        })
        .collect()
}

/// Read, checksum and verify errors, as the kernel's scrub counted them.
fn kernel_errors(report: &str) -> u64 {
    ["read_errors", "csum_errors", "verify_errors"]
        .iter()
        .map(|name| counter(report, name))
        .sum()
}

/// Hash every file the kernel reads from the mounted pool.
const HASH_EVERY_FILE: &str = "cd \"$MNT\"; find . -type f | sort | while read -r p; do \
     printf '%s\\t%s\\t%s\\n' \"${p#.}\" \"$(stat -c%s \"$p\")\" \
     \"$(sha256sum \"$p\" | cut -d' ' -f1)\"; done";

/// Damage two data sectors of `big.bin` and the fs tree's root block, the
/// copy `mirrors` names for each, and repair them; then every oracle.
fn a_damaged_pool_is_repaired(
    tag: &str,
    scratch: PoolScratch,
    manifest: &str,
    mirrors: [usize; 3],
) {
    let members = scratch.by_devid();
    let images = scratch.images();
    let images: Vec<&str> = images.iter().map(String::as_str).collect();

    // Where to damage, from the undamaged pool: the first sector of
    // big.bin, one a stripe element and a sector further on, and the fs
    // tree's root.
    let fs = scratch.readable();
    let sector = u64::from(fs.superblock().sectorsize);
    let (data, len) = first_extent(&fs, "/big.bin");
    let stripe_len = fs
        .chunk_map()
        .chunk_for(data)
        .expect("big.bin's chunk")
        .stripe_len;
    assert!(
        len >= stripe_len + 2 * sector,
        "{tag}: big.bin's first extent is {len} bytes"
    );
    let tree = fs.tree_root_public(FS_TREE).expect("the fs tree");
    let targets = [
        (ScrubTarget::Data, data, mirrors[0]),
        (ScrubTarget::Data, data + stripe_len + sector, mirrors[1]),
        (ScrubTarget::TreeBlock, tree, mirrors[2]),
    ];
    let places: Vec<(u64, u64)> = targets
        .iter()
        .map(|&(_, logical, mirror)| {
            let m = fs
                .chunk_map()
                .map_mirror(logical, mirror)
                .expect("map a copy");
            (m.devid, m.physical)
        })
        .collect();
    drop(fs);
    for &(devid, physical) in &places {
        damage(&members[&devid], physical);
    }

    let before = guest_kernel_pool_read(&images, "btrfs scrub start -B -R -r \"$MNT\" || true");
    let before = String::from_utf8_lossy(&before.out.stdout).into_owned();
    assert_eq!(
        kernel_errors(&before),
        targets.len() as u64,
        "{tag}: the kernel's scrub does not count the damage done here:\n{before}"
    );

    let report = scratch.writable().scrub_repair().expect("scrub repair");
    let mut want: Vec<(ScrubTarget, u64, usize)> = targets.to_vec();
    want.sort_by_key(|&(_, logical, mirror)| (logical, mirror));
    let found: Vec<(ScrubTarget, u64, usize)> = report
        .scrub
        .errors
        .iter()
        .map(|e| (e.what, e.logical, e.mirror))
        .collect();
    assert_eq!(found, want, "{tag}: {:#?}", report.scrub.errors);
    assert!(
        report.unrepairable.is_empty(),
        "{tag}: every damaged copy can be read some other way: {:#?}",
        report.unrepairable
    );
    // Exactly the damaged copies, and no parity: the data repaired is
    // what the parity was computed from.
    assert_eq!(report.repaired, want, "{tag}: each bad copy repaired once");

    for (source, member) in scratch.sources.iter().zip(&scratch.members) {
        assert!(
            same_bytes(source, member),
            "{tag}: {} is not, byte for byte, the member the kernel made",
            member.display()
        );
    }

    let again = scratch.readable().scrub().expect("scrub after the repair");
    assert!(again.errors.is_empty(), "{tag}: {:#?}", again.errors);

    let after = guest_kernel_pool_read(
        &images,
        &format!("btrfs scrub start -B -R -r \"$MNT\"; {HASH_EVERY_FILE}"),
    );
    assert_eq!(
        after.check_status, 0,
        "{tag}: btrfs check --readonly does not find the repaired pool clean:\n{}",
        after.check
    );
    let out = String::from_utf8_lossy(&after.out.stdout).into_owned();
    assert!(
        after.out.status.success(),
        "{tag}: the kernel could not scrub and read the repaired pool:\n{out}{}",
        String::from_utf8_lossy(&after.out.stderr)
    );
    assert_eq!(
        kernel_errors(&out),
        0,
        "{tag}: the kernel's scrub still finds errors after the repair:\n{out}"
    );
    assert_eq!(
        tab_rows(&out),
        files(manifest),
        "{tag}: the kernel reads the repaired pool back differently from what it wrote"
    );
}

#[test]
fn a_raid1_pool_damaged_on_both_members_is_repaired() {
    let scratch = PoolScratch::new("raid1", sources("btrfs-pool", &["a", "b"]));
    // The first sector's second copy, the next one's first: one on each
    // member.
    a_damaged_pool_is_repaired("raid1", scratch, "btrfs-pool.manifest", [1, 0, 1]);
}

#[test]
fn a_raid5_pool_is_repaired_from_parity() {
    let scratch = PoolScratch::new("raid5", sources("btrfs-raid5", &["1", "2", "3"]));
    a_damaged_pool_is_repaired("raid5", scratch, "btrfs-raid5.manifest", [0, 0, 0]);
}

#[test]
fn a_raid6_pool_is_repaired_from_parity() {
    let scratch = PoolScratch::new("raid6", sources("btrfs-raid6", &["1", "2", "3", "4"]));
    a_damaged_pool_is_repaired("raid6", scratch, "btrfs-raid6.manifest", [0, 0, 0]);
}

/// One parity element the guest damaged, from the fixture's manifest:
/// `(P or Q, devid, physical, stripe_len, sha256 the kernel's scrub
/// rewrote it to)`.
fn damaged_parity(profile: &str) -> Vec<(String, u64, u64, u64, String)> {
    let path = fixture(&format!("scrub/btrfs-scrub-{profile}.manifest"));
    let text = std::fs::read_to_string(path).expect("scrub parity manifest");
    let mut kernel: BTreeMap<String, String> = BTreeMap::new();
    let mut damaged = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        match f.first().copied() {
            Some("damaged") => damaged.push((
                f[1].to_string(),
                f[2].parse().expect("devid"),
                f[3].parse().expect("physical"),
                f[4].parse().expect("stripe_len"),
            )),
            Some("kernel") => {
                kernel.insert(f[1].to_string(), f[2].to_string());
            }
            _ => {}
        }
    }
    damaged
        .into_iter()
        .map(|(what, devid, physical, len)| {
            let sha = kernel
                .remove(&what)
                .expect("a kernel line for each element");
            (what, devid, physical, len, sha)
        })
        .collect()
}

fn a_damaged_parity_element_is_written_as_the_kernel_wrote_it(profile: &str, count: usize) {
    let suffixes: Vec<String> = (1..=count).map(|i| i.to_string()).collect();
    let suffixes: Vec<&str> = suffixes.iter().map(String::as_str).collect();
    let scratch = PoolScratch::new(
        &format!("parity-{profile}"),
        sources(&format!("scrub/btrfs-scrub-{profile}"), &suffixes),
    );
    let members = scratch.by_devid();
    let damaged = damaged_parity(profile);

    let report = scratch.writable().scrub_repair().expect("scrub repair");
    let found: BTreeSet<(usize, Option<(u64, u64)>)> = report
        .scrub
        .errors
        .iter()
        .map(|e| (e.mirror, e.device))
        .collect();
    let want: BTreeSet<(usize, Option<(u64, u64)>)> = damaged
        .iter()
        .map(|(what, devid, physical, _, _)| (usize::from(what == "Q"), Some((*devid, *physical))))
        .collect();
    assert_eq!(found, want, "{profile}: {:#?}", report.scrub.errors);
    assert!(
        report.unrepairable.is_empty(),
        "{profile}: the data each element is computed from verifies: {:#?}",
        report.unrepairable
    );
    let repaired: Vec<(ScrubTarget, u64, usize)> = report
        .scrub
        .errors
        .iter()
        .map(|e| (e.what, e.logical, e.mirror))
        .collect();
    assert!(
        repaired.iter().all(|r| r.0 == ScrubTarget::Parity),
        "{profile}: only parity is damaged: {repaired:?}"
    );
    assert_eq!(
        report.repaired, repaired,
        "{profile}: each bad element written once"
    );

    for (what, devid, physical, len, kernel) in &damaged {
        let mut buf = vec![0u8; *len as usize];
        FileDevice::open(&members[devid])
            .expect("open a member")
            .read_at(*physical, &mut buf)
            .expect("read a repaired element");
        assert_eq!(
            &sha256_hex(&buf),
            kernel,
            "{profile}: {what} at device {devid}, {physical} is not what the kernel's scrub wrote"
        );
    }

    let again = scratch.readable().scrub().expect("scrub after the repair");
    assert!(
        again.parity_stripes > 0,
        "{profile}: no full stripe was checked"
    );
    assert!(again.errors.is_empty(), "{profile}: {:#?}", again.errors);

    let images = scratch.images();
    let images: Vec<&str> = images.iter().map(String::as_str).collect();
    let after = guest_kernel_pool_read(&images, "btrfs scrub start -B -R -r \"$MNT\"");
    assert_eq!(
        after.check_status, 0,
        "{profile}: btrfs check --readonly does not find the repaired pool clean:\n{}",
        after.check
    );
    let out = String::from_utf8_lossy(&after.out.stdout).into_owned();
    assert!(
        after.out.status.success(),
        "{profile}: the kernel could not scrub the repaired pool:\n{out}{}",
        String::from_utf8_lossy(&after.out.stderr)
    );
    assert_eq!(kernel_errors(&out), 0, "{profile}:\n{out}");
}

#[test]
fn a_damaged_raid5_p_is_written_as_the_kernel_wrote_it() {
    a_damaged_parity_element_is_written_as_the_kernel_wrote_it("raid5", 3);
}

#[test]
fn a_damaged_raid6_p_and_q_are_written_as_the_kernel_wrote_them() {
    a_damaged_parity_element_is_written_as_the_kernel_wrote_it("raid6", 4);
}
