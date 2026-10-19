//! A write to an ordinary copy-on-write file lands in a new extent, and
//! the kernel and `btrfs check` both agree with the result (#61).
//!
//! # What this covers
//!
//! The first slice of copy-on-write file writing: a file that is
//! copy-on-write but not checksummed, which is what the kernel creates
//! on a volume mounted `-o nodatasum`. Each extent the write touches is
//! copied whole into a newly allocated one with the write applied, the
//! file's `EXTENT_DATA` item is pointed at the copy, the extent tree
//! records the new extent and forgets the old one, the free-space tree
//! and the block groups' `used` follow, and the transaction is committed.
//! A checksummed file gets a digest for every sector of each copy, and
//! the digests of the extents it replaced are removed (#261). An extent a
//! reflink or another item of the file also references is copied and
//! keeps those references (#261), and so is one a snapshot shares, which
//! the snapshot goes on reading (#443). A write into a hole adds an
//! extent, and so does one that starts at or before the end of a file
//! and grows it (#432).
//!
//! # The oracles
//!
//! The volume is made by `mkfs.btrfs` and filled by the kernel, in the
//! harness VM. After this crate writes it, `btrfs check` inspects the
//! whole volume — a leaked or doubly-referenced extent, a free-space tree
//! that disagrees with the extent tree, or a block group whose `used` is
//! wrong are each reported there and nowhere else — and then the kernel
//! mounts it and reads every byte back. `filefrag` asks the kernel where
//! each file's extent now lies, which is how the test knows the write was
//! copy-on-write and not in place.

use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, guest_kernel_read_ok, guest_kernel_write_ok, oracle,
    sha256_hex, temp_path,
};
use fs_core::{BlockDevice, FileDevice};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One extent of 64 KiB, unchecksummed.
const ONE: &str = "one.bin";
/// Two extents — 64 KiB, then 40,000 bytes appended in a later
/// transaction — so the file ends partway through its last sector.
const TWO: &str = "two.bin";
/// Written before the remount, so an ordinary checksummed file.
const SUMMED: &str = "summed.bin";

/// A scratch volume, removed when it drops — including on a panic.
struct Scratch(PathBuf);

impl Scratch {
    /// A fresh 256 MiB volume from `mkfs.btrfs`, filled by `script` run
    /// against a read-write kernel mount.
    fn new(name: &str, script: &str) -> Self {
        Self::with_csum(name, "crc32c", script)
    }

    /// As [`Scratch::new`], on a volume whose checksum algorithm is
    /// `algorithm` (`mkfs.btrfs --csum`; crc32c is its default).
    fn with_csum(name: &str, algorithm: &str, script: &str) -> Self {
        Self::made(name, algorithm, "no-holes", script)
    }

    fn made(name: &str, algorithm: &str, features: &str, script: &str) -> Self {
        Self::sized(name, algorithm, features, "16384", script)
    }

    /// As [`Scratch::made`], with tree blocks of `nodesize` bytes.
    fn sized(name: &str, algorithm: &str, features: &str, nodesize: &str, script: &str) -> Self {
        let dir = PathBuf::from(temp_path!("{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("fs.img");
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(256 << 20))
            .unwrap();
        let made = oracle("mkfs.btrfs")
            .args([
                "-q", "-f", "-s", "4096", "-n", nodesize, "--csum", algorithm, "-O", features,
            ])
            .arg(&image)
            .output();
        assert!(
            made.status.success(),
            "mkfs.btrfs --csum {algorithm} -O {features}: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        guest_kernel_write_ok(&image.to_string_lossy(), name, script);
        Scratch(dir)
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

fn mount_rw(image: &Path) -> Filesystem {
    let dev = Arc::new(FileDevice::open_rw(image).expect("open read-write"));
    Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount read-write")
}

fn ino_of(fs: &Filesystem, name: &str) -> u64 {
    fs.lookup_path(&format!("/{name}"))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .ino
}

/// What the kernel says about each named file: its SHA-256 and the
/// physical address of its first extent, from `filefrag`.
fn kernel_view(image: &Path, what: &str, names: &[&str]) -> BTreeMap<String, (String, String)> {
    let mut script = String::new();
    for name in names {
        script.push_str(&format!(
            "printf '%s %s %s\\n' {name} \"$(sha256sum \"$MNT/{name}\" | cut -d' ' -f1)\" \
             \"$(filefrag -v \"$MNT/{name}\" | awk '$1 == \"0:\" {{print $4}}')\"\n"
        ));
    }
    guest_kernel_read_ok(&image.to_string_lossy(), what, &script)
        .lines()
        .map(|line| {
            let mut words = line.split_whitespace();
            let name = words.next().unwrap_or_default().to_string();
            let sha = words.next().unwrap_or_default().to_string();
            let at = words.next().unwrap_or_default().to_string();
            (name, (sha, at))
        })
        .collect()
}

/// The fill every test here starts from.
const FILL: &str = "head -c 65536 /dev/urandom > \"$MNT/summed.bin\"\n\
     sync\n\
     mount -o remount,nodatasum \"$MNT\"\n\
     head -c 65536 /dev/urandom > \"$MNT/one.bin\"\n\
     sync\n\
     head -c 65536 /dev/urandom > \"$MNT/two.bin\"\n\
     sync\n\
     head -c 40000 /dev/urandom >> \"$MNT/two.bin\"\n\
     sync";

/// Writes inside one extent, across the boundary between two, and up to
/// the end of a file that stops partway through a sector — several
/// transactions on one mount, so each plans from what the last committed.
#[test]
fn a_copy_on_write_file_is_written_into_new_extents_the_checker_and_kernel_accept() {
    let scratch = Scratch::new("cow-write", FILL);
    let image = scratch.image();
    let before = kernel_view(&image, "before the write", &[ONE, TWO, SUMMED]);

    // (file, offset, bytes)
    let writes: Vec<(&str, u64, Vec<u8>)> = vec![
        (ONE, 5000, b"copy-on-write, inside one extent ".repeat(3)),
        (TWO, 61440, vec![0xa5; 8192]),
        (TWO, 104_536, vec![0x5a; 1000]),
    ];

    let mut fs = mount_rw(&image);
    let mut expected: BTreeMap<&str, Vec<u8>> = BTreeMap::new();
    for name in [ONE, TWO] {
        let ino = ino_of(&fs, name);
        let inode = fs.read_inode(ino).unwrap();
        assert_eq!(
            inode.flags & fs_btrfs::write::INODE_NODATACOW,
            0,
            "{name} must be copy-on-write for this to test anything"
        );
        assert_ne!(
            inode.flags & fs_btrfs::write::INODE_NODATASUM,
            0,
            "{name} must be nodatasum: the remount did not take"
        );
        expected.insert(name, fs.read_file(ino).unwrap());
    }
    assert_eq!(expected[TWO].len(), 105_536, "{TWO} is not the size made");

    let generation = fs.superblock().generation;
    for (name, offset, bytes) in &writes {
        let ino = ino_of(&fs, name);
        let n = fs
            .write(ino, *offset, bytes)
            .unwrap_or_else(|e| panic!("writing {} bytes at {offset} of {name}: {e}", bytes.len()));
        assert_eq!(n, bytes.len(), "a short write");
        let file = expected.get_mut(name).unwrap();
        file[*offset as usize..*offset as usize + bytes.len()].copy_from_slice(bytes);
        // Our own reading of what we committed, before the oracles'.
        assert_eq!(
            &fs.read_file(ino).unwrap(),
            file,
            "{name} reads back differently through this driver after the write"
        );
    }
    assert_eq!(
        fs.superblock().generation,
        generation + writes.len() as u64,
        "each write is one committed transaction"
    );
    drop(fs);

    assert_btrfs_check_clean(&image, "after copy-on-write writes");

    let after = kernel_view(&image, "after the write", &[ONE, TWO, SUMMED]);
    assert_eq!(
        after[SUMMED], before[SUMMED],
        "{SUMMED}, which nothing wrote, changed under the writes"
    );
    for name in [ONE, TWO] {
        assert_eq!(
            after[name].0,
            sha256_hex(&expected[name]),
            "the kernel reads back different bytes from {name} than were written"
        );
        assert_ne!(
            after[name].1, before[name].1,
            "{name}'s first extent is where it was, so the write was not copy-on-write"
        );
    }
}

/// The fill for the checksummed tests: two checksummed files written in
/// one transaction, so their extents are contiguous and the kernel is
/// free to pack both files' digests into one `EXTENT_CSUM` item, then a
/// second extent appended to the first file that ends partway through a
/// sector.
const SUMMED_FILL: &str = "head -c 65536 /dev/urandom > \"$MNT/a.bin\"\n\
     head -c 65536 /dev/urandom > \"$MNT/b.bin\"\n\
     sync\n\
     head -c 40000 /dev/urandom >> \"$MNT/a.bin\"\n\
     sync";

/// A checksummed file is written copy-on-write with a digest for every
/// sector of each copy, and the digests of the extents it replaced are
/// gone (#261) — on a volume of each digest width, crc32c's four bytes
/// and sha256's thirty-two.
///
/// `btrfs check --check-data-csum` reads every data sector on the volume
/// and compares it with the checksum tree, so a digest computed wrongly,
/// filed under the wrong address, left behind for a released extent or
/// missing for a new one is reported there. The kernel verifies each
/// sector it reads too — a mismatch is an I/O error, never wrong bytes —
/// and `btrfs scrub` makes it verify every sector of every file.
#[test]
fn a_checksummed_file_is_written_with_digests_the_checker_and_kernel_verify() {
    for algorithm in ["crc32c", "sha256"] {
        let scratch = Scratch::with_csum(
            &format!("cow-write-summed-{algorithm}"),
            algorithm,
            SUMMED_FILL,
        );
        let image = scratch.image();
        let before = kernel_view(&image, "before the write", &["a.bin", "b.bin"]);

        // (file, offset, bytes). Across a.bin's two extents first, then
        // inside its second only: a.bin's first extent then moves once,
        // and a second move could land back in the space the first one
        // released, which would leave nothing for `filefrag` to see.
        let writes: Vec<(&str, u64, Vec<u8>)> = vec![
            ("a.bin", 61440, vec![0xa5; 8192]),
            ("a.bin", 70000, b"checksummed, inside one extent ".repeat(3)),
            ("b.bin", 30000, vec![0x5a; 3000]),
        ];

        let mut fs = mount_rw(&image);
        let mut expected: BTreeMap<&str, Vec<u8>> = BTreeMap::new();
        for name in ["a.bin", "b.bin"] {
            let ino = ino_of(&fs, name);
            let inode = fs.read_inode(ino).unwrap();
            assert_eq!(
                inode.flags & fs_btrfs::write::INODE_NODATASUM,
                0,
                "{name} must be checksummed for this to test anything"
            );
            expected.insert(name, fs.read_file(ino).unwrap());
        }
        for (name, offset, bytes) in &writes {
            let ino = ino_of(&fs, name);
            let n = fs.write(ino, *offset, bytes).unwrap_or_else(|e| {
                panic!(
                    "{algorithm}: writing {} bytes at {offset} of {name}: {e}",
                    bytes.len()
                )
            });
            assert_eq!(n, bytes.len(), "a short write");
            let file = expected.get_mut(name).unwrap();
            file[*offset as usize..*offset as usize + bytes.len()].copy_from_slice(bytes);
            // This driver verifies the digests it reads, so this is our
            // own check of the digests we just wrote, before the oracles'.
            for other in ["a.bin", "b.bin"] {
                assert_eq!(
                    &fs.read_file(ino_of(&fs, other)).unwrap(),
                    &expected[other],
                    "{algorithm}: {other} reads back differently through this driver after \
                     writing {name}"
                );
            }
        }
        drop(fs);

        assert_btrfs_check_clean(&image, &format!("{algorithm}: after checksummed writes"));
        let checked = oracle("btrfs")
            .args(["check", "--readonly", "--check-data-csum"])
            .arg(&image)
            .output();
        assert_eq!(
            checked.status.code(),
            Some(0),
            "{algorithm}: btrfs check --check-data-csum finds data that disagrees with its \
             digests:\n{}{}",
            String::from_utf8_lossy(&checked.stdout),
            String::from_utf8_lossy(&checked.stderr)
        );

        let after = kernel_view(&image, "after the write", &["a.bin", "b.bin"]);
        for name in ["a.bin", "b.bin"] {
            assert_eq!(
                after[name].0,
                sha256_hex(&expected[name]),
                "{algorithm}: the kernel reads back different bytes from {name} than were \
                 written"
            );
            assert_ne!(
                after[name].1, before[name].1,
                "{algorithm}: {name}'s first extent is where it was, so the write was not \
                 copy-on-write"
            );
        }
        guest_kernel_write_ok(
            &image.to_string_lossy(),
            &format!("{algorithm}: scrub"),
            "btrfs scrub start -B \"$MNT\" >/dev/null",
        );
    }
}

/// The extent record at `bytenr`: its reference count, the root of each
/// data reference naming a tree (inline or keyed), and how many name a
/// parent tree block instead.
fn data_refs(image: &Path, bytenr: u64) -> (u64, Vec<String>, usize) {
    let extents = dump_tree(image, "2");
    let lines: Vec<&str> = extents.lines().collect();
    let record = format!("key ({bytenr} EXTENT_ITEM ");
    let at = lines
        .iter()
        .position(|l| l.contains(&record))
        .unwrap_or_else(|| panic!("no extent record at {bytenr}\n{extents}"));
    let words: Vec<&str> = lines[at + 1].split_whitespace().collect();
    let refs = words
        .iter()
        .position(|w| *w == "refs")
        .and_then(|i| words.get(i + 1)?.parse().ok())
        .unwrap_or_else(|| panic!("no refs in {:?}", lines[at + 1]));
    // The record, then its keyed references: every item keyed by the
    // extent's address.
    let own = format!("key ({bytenr} ");
    let body: Vec<&str> = lines[at + 1..]
        .iter()
        .take_while(|l| !l.contains(" key (") || l.contains(&own))
        .copied()
        .collect();
    let roots = body
        .iter()
        .filter(|l| l.contains("extent data backref"))
        .filter_map(|l| {
            let words: Vec<&str> = l.split_whitespace().collect();
            let i = words.iter().position(|w| *w == "root")?;
            Some(words.get(i + 1)?.to_string())
        })
        .collect();
    let shared = body
        .iter()
        .filter(|l| l.contains("shared data backref"))
        .count();
    (refs, roots, shared)
}

/// Whether `root`, as `dump-tree` prints a data reference's root, is the
/// top-level subvolume.
fn top_level(root: &str) -> bool {
    root == "5" || root == "FS_TREE"
}

/// A write into an extent a snapshot shares copies it (#443): the top
/// level's item is pointed at the copy, and the old extent keeps the
/// snapshot's reference and loses only the top level's, so the snapshot
/// reads its bytes unchanged.
///
/// A tree one leaf tall is copied whole by the snapshot, so each tree's
/// item counts in the extent's record. A taller tree shares its leaves,
/// and the record counts one reference for both; copying the leaf out of
/// the snapshot's way gives the extent a reference naming the old leaf,
/// as the kernel's copy-on-write does. Each is taken read-only and
/// writable. The kernel made the files and the snapshots; `btrfs check
/// --check-data-csum`, a kernel scrub and the kernel's reads judge the
/// result, and `dump-tree` the references.
#[test]
fn a_copy_on_write_extent_a_snapshot_shares_is_copied_and_the_snapshot_keeps_it() {
    const SNAP: &str = "snap/one.bin";
    for (case, tall, readonly) in [
        ("leaf-ro", false, true),
        ("leaf-rw", false, false),
        ("tall-ro", true, true),
        ("tall-rw", true, false),
    ] {
        let many = if tall {
            "mkdir \"$MNT/many\"\n\
             (cd \"$MNT/many\" && seq -f 'f%05g' 1 3000 | xargs touch)\n"
        } else {
            ""
        };
        let flag = if readonly { "-r " } else { "" };
        let scratch = Scratch::new(
            &format!("cow-write-snapshot-{case}"),
            &format!(
                "head -c 65536 /dev/urandom > \"$MNT/one.bin\"\n\
                 {many}sync\n\
                 btrfs subvolume snapshot {flag}\"$MNT\" \"$MNT/snap\" >/dev/null\n\
                 sync"
            ),
        );
        let image = scratch.image();
        let before = kernel_view(&image, case, &[ONE, SNAP]);

        let mut fs = mount_rw(&image);
        let ino = ino_of(&fs, ONE);
        let (_, old, _) = extent_kind_and_refs(&image, ino, 0);
        let (refs, roots, _) = data_refs(&image, old);
        // THE CASE IS REAL: a leaf tree's snapshot counts its own
        // reference; a tall tree's shares the leaf, and the record counts
        // the top level's alone.
        let want = if tall { 1 } else { 2 };
        assert_eq!(
            refs, want,
            "{case}: the extent at {old} has {refs} references ({roots:?}), so the fixture \
             no longer holds the case"
        );
        let mut expected = fs.read_file(ino).unwrap();
        let patch = b"written over what the snapshot keeps";
        fs.write(ino, 4096, patch)
            .unwrap_or_else(|e| panic!("{case}: a write into a snapshot-shared extent: {e}"));
        expected[4096..4096 + patch.len()].copy_from_slice(patch);
        drop(fs);

        assert_btrfs_check_clean(&image, case);
        let out = oracle("btrfs")
            .args(["check", "--readonly", "--check-data-csum"])
            .arg(&image)
            .output();
        assert!(
            out.status.success(),
            "{case}: btrfs check --check-data-csum finds data that disagrees with its \
             digests:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );

        let after = kernel_view(&image, case, &[ONE, SNAP]);
        assert_eq!(
            after[ONE].0,
            sha256_hex(&expected),
            "{case}: the kernel reads back different bytes from the top level than were written"
        );
        assert_ne!(
            after[ONE].1, before[ONE].1,
            "{case}: the top level's extent is where it was, so the write was not copy-on-write"
        );
        assert_eq!(
            after[SNAP], before[SNAP],
            "{case}: the snapshot's file changed, in its bytes or where they lie"
        );

        let (_, new, _) = extent_kind_and_refs(&image, ino, 0);
        assert_ne!(
            new, old,
            "{case}: the top level's item still names the old extent"
        );
        let (refs, roots, shared) = data_refs(&image, old);
        assert!(
            refs == 1 && !roots.iter().any(|r| top_level(r)),
            "{case}: the old extent should keep the snapshot's one reference and lose the top \
             level's: refs {refs}, roots {roots:?}, {shared} by parent"
        );
        let (refs, roots, shared) = data_refs(&image, new);
        assert!(
            refs == 1 && roots.len() == 1 && top_level(&roots[0]) && shared == 0,
            "{case}: the copy should hold the top level's reference alone: refs {refs}, roots \
             {roots:?}, {shared} by parent"
        );

        guest_kernel_write_ok(
            &image.to_string_lossy(),
            &format!("{case}: scrub"),
            "btrfs scrub start -B \"$MNT\" >/dev/null",
        );
    }
}

/// One extent as the kernel's `FIEMAP` reports it, through `filefrag -v`:
/// where in the file it begins and where on the device, both in blocks,
/// and whether the kernel calls it shared.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mapped {
    logical: u64,
    physical: u64,
    shared: bool,
}

/// What the kernel says about each named file: its SHA-256, and every
/// extent `filefrag -v` lists for it.
fn kernel_map(image: &Path, what: &str, names: &[&str]) -> BTreeMap<String, (String, Vec<Mapped>)> {
    let mut script = String::new();
    for name in names {
        script.push_str(&format!(
            "printf 'file %s %s\\n' {name} \"$(sha256sum \"$MNT/{name}\" | cut -d' ' -f1)\"\n\
             filefrag -v \"$MNT/{name}\" | awk '$1 ~ /^[0-9]+:$/ {{ \
               sub(/\\.\\./, \"\", $2); sub(/\\.\\./, \"\", $4); \
               print \"extent\", $2, $4, ($NF ~ /shared/ ? \"shared\" : \"own\") }}'\n"
        ));
    }
    let mut out: BTreeMap<String, (String, Vec<Mapped>)> = BTreeMap::new();
    let mut current = String::new();
    for line in guest_kernel_read_ok(&image.to_string_lossy(), what, &script).lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["file", name, sha] => {
                current = name.to_string();
                out.insert(current.clone(), (sha.to_string(), Vec::new()));
            }
            ["extent", logical, physical, flag] => {
                let parse = |w: &str| {
                    w.parse::<u64>()
                        .unwrap_or_else(|e| panic!("{what}: filefrag printed {w:?}: {e}"))
                };
                out.get_mut(&current)
                    .unwrap_or_else(|| panic!("{what}: an extent before any file: {line}"))
                    .1
                    .push(Mapped {
                        logical: parse(logical),
                        physical: parse(physical),
                        shared: *flag == "shared",
                    });
            }
            _ => panic!("{what}: an unexpected line from the guest: {line:?}"),
        }
    }
    out
}

/// The fill for the shared-extent test. `orig.bin` and its reflink
/// `twin.bin` reference one extent, recorded with a reference from each.
/// `split.bin` and `whole.bin` are each one 128 KiB extent whose middle
/// the kernel then overwrote: the item before the overwrite and the item
/// after it both reference the original extent, at one reference offset,
/// so its record counts that reference twice. All four are checksummed.
const SHARED_FILL: &str = "head -c 65536 /dev/urandom > \"$MNT/orig.bin\"\n\
     head -c 131072 /dev/urandom > \"$MNT/split.bin\"\n\
     head -c 131072 /dev/urandom > \"$MNT/whole.bin\"\n\
     sync\n\
     cp --reflink=always \"$MNT/orig.bin\" \"$MNT/twin.bin\"\n\
     dd if=/dev/urandom of=\"$MNT/split.bin\" bs=4096 seek=8 count=4 conv=notrunc \
        status=none\n\
     dd if=/dev/urandom of=\"$MNT/whole.bin\" bs=4096 seek=8 count=4 conv=notrunc \
        status=none\n\
     sync";

/// An extent another reference shares is copied, and keeps the other
/// references (#261): a reflink's, and another item of the same file
/// outside the write. An extent whose every reference is an item the
/// write replaces is released once, however many of its items named it.
///
/// `btrfs check` counts every extent's references against the items that
/// name it, so a reference dropped that should have stayed, one left that
/// should have gone, or an extent freed while something still reads it is
/// reported there. The kernel's `FIEMAP` says which extents it considers
/// shared, which is how the test knows the twin's reference left the
/// original's extent and the original kept it; `btrfs check
/// --check-data-csum` and a kernel scrub verify every digest, the kept
/// extents' among them.
#[test]
fn an_extent_a_reflink_or_another_item_shares_is_copied_and_keeps_its_other_references() {
    let scratch = Scratch::new("cow-write-shared", SHARED_FILL);
    let image = scratch.image();
    let names = ["orig.bin", "twin.bin", "split.bin", "whole.bin"];
    let before = kernel_map(&image, "before the write", &names);

    // The premises, from the kernel: the twins share one extent, and each
    // split file's third extent is the rest of its first, twelve blocks on.
    let first = |view: &BTreeMap<String, (String, Vec<Mapped>)>, name: &str| {
        view[name]
            .1
            .first()
            .cloned()
            .unwrap_or_else(|| panic!("{name} has no extents"))
    };
    assert!(
        first(&before, "twin.bin").shared
            && first(&before, "twin.bin").physical == first(&before, "orig.bin").physical,
        "the reflink did not share orig.bin's extent: {before:?}"
    );
    for name in ["split.bin", "whole.bin"] {
        let extents = &before[name].1;
        assert!(
            extents.len() == 3
                && extents[0].logical == 0
                && extents[2].logical == 12
                && extents[2].physical == extents[0].physical + 12,
            "{name} is not one extent split around a kernel overwrite: {extents:?}"
        );
    }

    let mut fs = mount_rw(&image);
    let mut expected: BTreeMap<&str, Vec<u8>> = BTreeMap::new();
    for name in names {
        expected.insert(name, fs.read_file(ino_of(&fs, name)).unwrap());
    }
    // (file, offset, bytes): one of the twins; the first item of a split
    // file only, so the item after the overwrite keeps the extent; and
    // every item of the other, so the extent goes, released once.
    let writes: Vec<(&str, u64, Vec<u8>)> = vec![
        ("twin.bin", 5000, b"a reflink, copied on write ".repeat(100)),
        ("split.bin", 4096, vec![0xa5; 4096]),
        ("whole.bin", 16384, vec![0x5a; 81920]),
    ];
    for (name, offset, bytes) in &writes {
        let ino = ino_of(&fs, name);
        let n = fs
            .write(ino, *offset, bytes)
            .unwrap_or_else(|e| panic!("writing {} bytes at {offset} of {name}: {e}", bytes.len()));
        assert_eq!(n, bytes.len(), "a short write");
        let file = expected.get_mut(name).unwrap();
        file[*offset as usize..*offset as usize + bytes.len()].copy_from_slice(bytes);
        for other in names {
            assert_eq!(
                &fs.read_file(ino_of(&fs, other)).unwrap(),
                &expected[other],
                "{other} reads back differently through this driver after writing {name}"
            );
        }
    }
    drop(fs);

    assert_btrfs_check_clean(&image, "after writes into shared extents");
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum finds data that disagrees with its digests:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );

    let after = kernel_map(&image, "after the write", &names);
    for name in names {
        assert_eq!(
            after[name].0,
            sha256_hex(&expected[name]),
            "the kernel reads back different bytes from {name} than were written"
        );
    }
    assert_eq!(
        after["orig.bin"].1,
        vec![Mapped {
            shared: false,
            ..first(&before, "orig.bin")
        }],
        "orig.bin's extent should be where it was, and no longer shared once its twin was copied"
    );
    assert_ne!(
        first(&after, "twin.bin").physical,
        first(&before, "twin.bin").physical,
        "twin.bin still starts in the extent it shared, so the write was not copy-on-write"
    );
    assert_ne!(
        first(&after, "split.bin").physical,
        first(&before, "split.bin").physical,
        "split.bin's first item was not copied"
    );
    // Where, not whether shared: the two items naming it may have counted
    // as sharing it, and only one names it now.
    let tail = |view: &BTreeMap<String, (String, Vec<Mapped>)>| {
        view["split.bin"].1.last().map(|m| (m.logical, m.physical))
    };
    assert_eq!(
        tail(&after),
        tail(&before),
        "split.bin's last item should still read the extent the write left it"
    );
    assert_ne!(
        first(&after, "whole.bin").physical,
        first(&before, "whole.bin").physical,
        "whole.bin's first item was not copied"
    );

    // The original, now its extent's one reference, is released when
    // written: the record the twin's write left must be exactly that.
    let mut fs = mount_rw(&image);
    let ino = ino_of(&fs, "orig.bin");
    fs.write(ino, 0, &[0x3c; 4096])
        .unwrap_or_else(|e| panic!("writing orig.bin after its twin was copied: {e}"));
    expected.get_mut("orig.bin").unwrap()[..4096].copy_from_slice(&[0x3c; 4096]);
    drop(fs);
    assert_btrfs_check_clean(
        &image,
        "after writing the last reference of a shared extent",
    );
    let last = kernel_map(&image, "after writing orig.bin", &["orig.bin"]);
    assert_eq!(
        last["orig.bin"].0,
        sha256_hex(&expected["orig.bin"]),
        "the kernel reads back different bytes from orig.bin than were written"
    );
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub after writes into shared extents",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );
}

/// `holey.bin`: 64 KiB of data, a 64 KiB hole the kernel leaves with no
/// item at all on a `no-holes` volume (mkfs's default), then 64 KiB more,
/// written in one transaction so the hole is the file's only gap.
///
/// `control.bin` is made the same way, and then the kernel itself writes
/// the same 9000 bytes of 0xc3 at offset 70000 into its hole, in a later
/// transaction. It is the reference the driver's write is held to: the
/// `nbytes`, bytes and extent layout the kernel produced for the same
/// write into the same hole.
const HOLE_FILL: &str = "head -c 65536 /dev/urandom > \"$MNT/holey.bin\"\n\
     dd if=/dev/urandom of=\"$MNT/holey.bin\" bs=65536 seek=2 count=1 conv=notrunc \
     status=none\n\
     head -c 65536 /dev/urandom > \"$MNT/control.bin\"\n\
     dd if=/dev/urandom of=\"$MNT/control.bin\" bs=65536 seek=2 count=1 conv=notrunc \
     status=none\n\
     sync\n\
     head -c 9000 /dev/zero | tr '\\000' '\\303' \
     | dd of=\"$MNT/control.bin\" bs=1 seek=70000 conv=notrunc status=none\n\
     sync";

/// A write into a hole of an existing file allocates a new extent,
/// inserts an `EXTENT_DATA` item for it and moves the inode's `nbytes`
/// by the sectors it adds, leaving `size` alone (#261).
///
/// The write starts and ends partway through a sector, so the extent
/// holds zeros before and after the bytes written, which is what the
/// hole read as. `btrfs check --check-data-csum` sees an extent with no
/// record, a record with no item, a wrong `nbytes` or a missing digest;
/// the kernel reads every byte back, and `filefrag` reports an extent
/// where the hole was.
#[test]
fn a_write_into_a_hole_adds_an_extent_the_checker_and_kernel_accept() {
    const HOLEY: &str = "holey.bin";
    const CONTROL: &str = "control.bin";
    let scratch = Scratch::new("cow-write-hole", HOLE_FILL);
    let image = scratch.image();
    let before = kernel_map(&image, "before the hole write", &[HOLEY]);
    assert_eq!(
        before[HOLEY].1.len(),
        2,
        "{HOLEY} must be two extents around one hole for this to test anything: {:?}",
        before[HOLEY].1
    );

    let mut fs = mount_rw(&image);
    let ino = ino_of(&fs, HOLEY);
    let inode = fs.read_inode(ino).unwrap();
    assert_eq!(inode.size, 196_608, "{HOLEY} is not the size made");
    assert_eq!(
        inode.nbytes, 131_072,
        "{HOLEY} must hold 128 KiB, its hole none"
    );
    let mut expected = fs.read_file(ino).unwrap();
    assert!(
        expected[65536..131_072].iter().all(|&b| b == 0),
        "{HOLEY}'s hole does not read as zeros"
    );

    // What the kernel made of the same write into the same hole.
    let control_ino = ino_of(&fs, CONTROL);
    let control = fs.read_inode(control_ino).unwrap();
    assert_eq!(control.size, 196_608, "{CONTROL} is not the size made");
    let control_bytes = fs.read_file(control_ino).unwrap();

    let (offset, bytes) = (70_000u64, vec![0xc3u8; 9000]);
    assert_eq!(
        &control_bytes[offset as usize..offset as usize + bytes.len()],
        &bytes[..],
        "the kernel's control write did not land where this test writes"
    );
    let generation = fs.superblock().generation;
    let n = fs
        .write(ino, offset, &bytes)
        .unwrap_or_else(|e| panic!("writing {} bytes into {HOLEY}'s hole: {e}", bytes.len()));
    assert_eq!(n, bytes.len(), "a short write");
    expected[offset as usize..offset as usize + bytes.len()].copy_from_slice(&bytes);
    assert_eq!(
        fs.superblock().generation,
        generation + 1,
        "one committed transaction"
    );
    let inode = fs.read_inode(ino).unwrap();
    assert_eq!(
        inode.size, 196_608,
        "a write inside the file moved its size"
    );
    // The kernel, not this test's arithmetic, says how many sectors the
    // write allocates: its own write into the same hole of `control.bin`.
    assert!(
        inode.nbytes > 131_072 && inode.nbytes.is_multiple_of(4096),
        "nbytes did not grow by whole sectors: {}",
        inode.nbytes
    );
    assert_eq!(
        inode.nbytes, control.nbytes,
        "nbytes is not what the kernel's own write into the same hole gave {CONTROL}"
    );
    assert_eq!(
        expected[65536..131_072],
        control_bytes[65536..131_072],
        "the hole does not read as the kernel's control write left it"
    );
    assert_eq!(
        fs.read_file(ino).unwrap(),
        expected,
        "{HOLEY} reads back differently through this driver after the write"
    );
    drop(fs);

    assert_btrfs_check_clean(&image, "after a write into a hole");
    // A scrub needs a read-write mount: on a read-only one the kernel
    // refuses it with EROFS, as the other scrubs in this file show.
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub after a write into a hole",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );

    let blocks = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "the kernel's nbytes after a write into a hole",
        &format!("stat -c '%b %B' \"$MNT/{HOLEY}\" \"$MNT/{CONTROL}\""),
    );
    let blocks: Vec<u64> = blocks
        .lines()
        .map(|line| {
            let words: Vec<u64> = line
                .split_whitespace()
                .map(|w| {
                    w.parse()
                        .unwrap_or_else(|e| panic!("stat printed {line:?}: {e}"))
                })
                .collect();
            assert_eq!(words.len(), 2, "stat printed {line:?}");
            words[0] * words[1]
        })
        .collect();

    let after = kernel_map(&image, "after the hole write", &[HOLEY, CONTROL]);
    // The values every comparison below rests on, written before any of
    // them can fail, into the directory CI keeps as the logs artifact.
    let starts = |name: &str| {
        after[name]
            .1
            .iter()
            .map(|m| m.logical.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    let report = Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp/logs/hole-write-oracle.json");
    std::fs::create_dir_all(report.parent().unwrap()).unwrap();
    std::fs::write(
        &report,
        format!(
            "{{\"image_sha256\":\"{}\",\"driver_nbytes\":{},\"control_nbytes\":{},\
             \"kernel_stat_bytes\":[{}],\"holey_sha256\":\"{}\",\"control_sha256\":\"{}\",\
             \"holey_extent_starts\":[{}],\"control_extent_starts\":[{}]}}\n",
            sha256_hex(&std::fs::read(&image).unwrap()),
            inode.nbytes,
            control.nbytes,
            blocks
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(","),
            after[HOLEY].0,
            after[CONTROL].0,
            starts(HOLEY),
            starts(CONTROL),
        ),
    )
    .unwrap();
    assert_eq!(
        blocks,
        vec![inode.nbytes, control.nbytes],
        "the kernel's stat of {HOLEY} and {CONTROL} disagrees with nbytes"
    );
    assert_eq!(
        after[HOLEY].0,
        sha256_hex(&expected),
        "the kernel reads back different bytes from {HOLEY} than were written"
    );
    assert_eq!(
        after[HOLEY].1.len(),
        3,
        "the kernel does not see a new extent where the hole was: {:?}",
        after[HOLEY].1
    );
    assert_eq!(
        after[HOLEY].1.iter().map(|m| m.logical).collect::<Vec<_>>(),
        after[CONTROL]
            .1
            .iter()
            .map(|m| m.logical)
            .collect::<Vec<_>>(),
        "{HOLEY}'s extents do not begin where the kernel's own write put {CONTROL}'s: {:?} {:?}",
        after[HOLEY].1,
        after[CONTROL].1
    );
    for (old, new) in before[HOLEY]
        .1
        .iter()
        .zip([&after[HOLEY].1[0], &after[HOLEY].1[2]])
    {
        assert_eq!(
            old.physical, new.physical,
            "an extent the write did not touch moved"
        );
    }
}

/// The explicit holes reported by btrfs-progs, after checking that all
/// items tile the file without a gap or overlap. Data addresses differ
/// between the driver's file and the independently written control.
fn explicit_holes(image: &Path, ino: u64) -> Vec<(u64, u64)> {
    explicit_holes_to(image, ino, 196_608)
}

/// As [`explicit_holes`], for a file whose items end at `to`.
fn explicit_holes_to(image: &Path, ino: u64, to: u64) -> Vec<(u64, u64)> {
    extent_layout_to(image, ino, to)
        .into_iter()
        .filter_map(|(offset, len, hole)| hole.then_some((offset, len)))
        .collect()
}

/// Every explicit item's logical span and whether it is a hole, with
/// complete coverage checked independently by btrfs-progs.
fn extent_layout_to(image: &Path, ino: u64, to: u64) -> Vec<(u64, u64, bool)> {
    let tree = dump_tree(image, "5");
    let lines: Vec<&str> = tree.lines().collect();
    let needle = format!("key ({ino} EXTENT_DATA ");
    let mut end = 0;
    let mut layout = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        let Some((_, key)) = line.split_once(&needle) else {
            continue;
        };
        let offset: u64 = key.split(')').next().unwrap().parse().unwrap();
        let body: Vec<&str> = lines[at + 1..]
            .iter()
            .copied()
            .take_while(|l| !l.contains(" key ("))
            .collect();
        let field = |phrase: &str, word: &str| -> u64 {
            let line = body.iter().find(|l| l.contains(phrase)).unwrap();
            let mut words = line.split_whitespace();
            assert!(words.any(|w| w == word), "no {word} in {line}");
            words.next().unwrap().parse().unwrap()
        };
        let len = field("extent data offset", "nr");
        assert_eq!(
            offset, end,
            "inode {ino}: gap or overlap at {offset}\n{tree}"
        );
        assert!(len > 0, "inode {ino}: empty extent at {offset}");
        end = offset + len;
        layout.push((offset, len, field("extent data disk byte", "byte") == 0));
    }
    assert_eq!(end, to, "inode {ino}: incomplete extent coverage\n{tree}");
    layout
}

/// Without NO_HOLES, a hole is an item that must be trimmed, split or
/// replaced (#434). Cover each shape, unaligned writes, a write across
/// either data boundary, and a second fill into a previously split hole.
/// The kernel performs identical writes to independent control files;
/// btrfs-progs compares the remaining holes and validates every checksum.
#[test]
fn explicit_hole_fills_keep_items_the_checker_and_kernel_accept() {
    let names = ["start", "middle", "end", "whole", "left", "right", "again"];
    let writes = [
        ("start", 65_536u64, 9000usize),
        ("middle", 70_000, 9000),
        ("end", 122_000, 9072),
        ("whole", 65_536, 65_536),
        ("left", 64_536, 18_000),
        ("right", 122_000, 18_000),
        ("again", 70_000, 9000),
        ("again", 100_000, 9000),
    ];
    let mut script = String::new();
    for name in names {
        script.push_str(&format!(
            "for f in {name} {name}-control; do\n\
             head -c 65536 /dev/zero | tr '\\000' '\\151' > \"$MNT/$f\"\n\
             head -c 65536 /dev/zero | tr '\\000' '\\226' \
             | dd of=\"$MNT/$f\" bs=65536 seek=2 conv=notrunc status=none\n\
             done\n"
        ));
    }
    script.push_str("sync\n");
    for (name, offset, len) in writes {
        script.push_str(&format!(
            "head -c {len} /dev/zero | tr '\\000' '\\303' \
             | dd of=\"$MNT/{name}-control\" bs=1 seek={offset} conv=notrunc status=none\n\
             sync\n"
        ));
    }
    let scratch = Scratch::made("cow-write-explicit-holes", "crc32c", "^no-holes", &script);
    let image = scratch.image();
    let mut fs = mount_rw(&image);
    assert!(!fs.superblock().has_no_holes(), "mkfs enabled NO_HOLES");
    let mut expected = BTreeMap::new();
    let mut inodes = BTreeMap::new();
    for name in names {
        let ino = ino_of(&fs, name);
        inodes.insert(name, (ino, ino_of(&fs, &format!("{name}-control"))));
        assert_eq!(
            explicit_holes(&image, ino),
            vec![(65_536, 65_536)],
            "{name}: the kernel must have recorded an explicit hole"
        );
        let mut bytes = vec![0x69; 196_608];
        bytes[65_536..131_072].fill(0);
        bytes[131_072..].fill(0x96);
        assert_eq!(fs.read_file(ino).unwrap(), bytes);
        assert_eq!(fs.read_inode(ino).unwrap().nbytes, 131_072);
        expected.insert(name, bytes);
    }
    assert_btrfs_check_clean(&image, "before explicit hole fills");
    for (name, offset, len) in writes {
        let ino = inodes[name].0;
        let bytes = vec![0xc3; len];
        assert_eq!(
            fs.write(ino, offset, &bytes)
                .unwrap_or_else(|e| panic!("filling {name}'s hole at {offset}: {e}")),
            len
        );
        expected.get_mut(name).unwrap()[offset as usize..offset as usize + len]
            .copy_from_slice(&bytes);
    }
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert!(
        checked.status.success(),
        "btrfs check after explicit hole fills:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    for name in names {
        let (ino, control) = inodes[name];
        let ours = fs.read_inode(ino).unwrap();
        let theirs = fs.read_inode(control).unwrap();
        assert_eq!((ours.size, ours.nbytes), (theirs.size, theirs.nbytes));
        assert_eq!(fs.read_file(ino).unwrap(), expected[name]);
        assert_eq!(
            explicit_holes(&image, ino),
            explicit_holes(&image, control),
            "{name}: remaining holes differ from the kernel's control"
        );
    }
    drop(fs);
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub after explicit hole fills",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );
    let all_names: Vec<String> = names
        .iter()
        .flat_map(|n| [n.to_string(), format!("{n}-control")])
        .collect();
    let refs: Vec<&str> = all_names.iter().map(String::as_str).collect();
    let seen = kernel_view(&image, "read back explicit hole fills", &refs);
    for name in names {
        assert_eq!(
            seen[name].0,
            sha256_hex(&expected[name]),
            "{name}: wrong bytes"
        );
        assert_eq!(seen[name].0, seen[&format!("{name}-control")].0);
    }
}

/// `mid.bin` ends partway through a sector, `edge.bin` on a sector
/// boundary, both checksummed. Each `*-control.bin` is written by the same
/// `tee`, so it holds the same bytes in extents of its own (a `cp` could
/// reflink them), and then the kernel itself makes the writes
/// [`a_write_past_the_end_grows_the_file_the_checker_and_kernel_accept`]
/// makes through this driver, at the same offsets, in transactions of its
/// own: the `size`, `nbytes` and bytes the driver's appends are held to.
const GROW_FILL: &str =
    "head -c 40000 /dev/urandom | tee \"$MNT/mid.bin\" > \"$MNT/mid-control.bin\"\n\
     head -c 65536 /dev/urandom | tee \"$MNT/edge.bin\" > \"$MNT/edge-control.bin\"\n\
     sync\n\
     head -c 9000 /dev/zero | tr '\\000' '\\303' \
     | dd of=\"$MNT/mid-control.bin\" bs=1 seek=40000 conv=notrunc status=none\n\
     sync\n\
     head -c 100 /dev/zero | tr '\\000' '\\151' \
     | dd of=\"$MNT/mid-control.bin\" bs=1 seek=49000 conv=notrunc status=none\n\
     sync\n\
     head -c 9000 /dev/zero | tr '\\000' '\\074' \
     | dd of=\"$MNT/edge-control.bin\" bs=1 seek=64536 conv=notrunc status=none\n\
     sync\n\
     head -c 5000 /dev/zero | tr '\\000' '\\226' \
     | dd of=\"$MNT/edge-control.bin\" bs=1 seek=73536 conv=notrunc status=none\n\
     sync";

/// A write that starts at or before the end of a file and ends past it
/// grows the file (#432): the sectors past the old last extent become a
/// new extent with an item of its own, `size` moves to the write's end and
/// `nbytes` by the sectors added, in one committed transaction.
///
/// The writes cover an append exactly at an end partway through a sector,
/// one that stays inside that last sector and so adds none, one straddling
/// an end on a sector boundary, and an append to what an earlier append
/// committed. The kernel made the same writes into the controls, so the
/// expected `size` and `nbytes` are its own, not this test's arithmetic.
/// `btrfs check --check-data-csum` sees a digest missing for a new sector,
/// a record with no item or a wrong `nbytes`; a scrub makes the kernel
/// verify every sector, and it reads every byte back.
#[test]
fn a_write_past_the_end_grows_the_file_the_checker_and_kernel_accept() {
    const MID: &str = "mid.bin";
    const EDGE: &str = "edge.bin";
    let control = |name: &str| name.replace(".bin", "-control.bin");
    let scratch = Scratch::new("cow-write-grow", GROW_FILL);
    let image = scratch.image();

    let mut fs = mount_rw(&image);
    let mut expected: BTreeMap<&str, Vec<u8>> = BTreeMap::new();
    for (name, size) in [(MID, 40_000u64), (EDGE, 65_536)] {
        let ino = ino_of(&fs, name);
        let inode = fs.read_inode(ino).unwrap();
        assert_eq!(inode.size, size, "{name} is not the size made");
        assert_eq!(
            inode.flags & fs_btrfs::write::INODE_NODATASUM,
            0,
            "{name} must be checksummed for this to test anything"
        );
        expected.insert(name, fs.read_file(ino).unwrap());
    }

    // (file, offset, bytes), the controls' writes in the same order.
    let writes: Vec<(&str, u64, Vec<u8>)> = vec![
        (MID, 40_000, vec![0xc3; 9000]),
        (MID, 49_000, vec![0x69; 100]),
        (EDGE, 64_536, vec![0x3c; 9000]),
        (EDGE, 73_536, vec![0x96; 5000]),
    ];
    let generation = fs.superblock().generation;
    for (name, offset, bytes) in &writes {
        let ino = ino_of(&fs, name);
        let n = fs.write(ino, *offset, bytes).unwrap_or_else(|e| {
            panic!(
                "writing {} bytes at {offset} of {name}, past its end: {e}",
                bytes.len()
            )
        });
        assert_eq!(n, bytes.len(), "a short write");
        let file = expected.get_mut(name).unwrap();
        let end = *offset as usize + bytes.len();
        if file.len() < end {
            file.resize(end, 0);
        }
        file[*offset as usize..end].copy_from_slice(bytes);
        assert_eq!(
            fs.read_inode(ino).unwrap().size,
            end as u64,
            "{name}'s size is not the end of the write"
        );
        // Our own reading of what we committed, before the oracles'.
        assert_eq!(
            &fs.read_file(ino).unwrap(),
            file,
            "{name} reads back differently through this driver after the write"
        );
    }
    assert_eq!(
        fs.superblock().generation,
        generation + writes.len() as u64,
        "each write is one committed transaction"
    );
    for name in [MID, EDGE] {
        let ours = fs.read_inode(ino_of(&fs, name)).unwrap();
        let theirs = fs.read_inode(ino_of(&fs, &control(name))).unwrap();
        assert_eq!(
            (ours.size, ours.nbytes),
            (theirs.size, theirs.nbytes),
            "{name}'s size and nbytes are not what the kernel's own writes gave its control"
        );
    }
    drop(fs);

    assert_btrfs_check_clean(&image, "after writes past the end");
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum finds data that disagrees with its digests:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub after writes past the end",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );

    let names = [MID, EDGE, "mid-control.bin", "edge-control.bin"];
    let mut script = String::new();
    for name in names {
        script.push_str(&format!("stat -c '{name} %s %b %B' \"$MNT/{name}\"\n"));
    }
    let stats: BTreeMap<String, (u64, u64)> = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "the kernel's stat after writes past the end",
        &script,
    )
    .lines()
    .map(|line| {
        let words: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(words.len(), 4, "stat printed {line:?}");
        let n = |w: &str| -> u64 {
            w.parse()
                .unwrap_or_else(|e| panic!("stat printed {line:?}: {e}"))
        };
        (
            words[0].to_string(),
            (n(words[1]), n(words[2]) * n(words[3])),
        )
    })
    .collect();
    let after = kernel_view(&image, "after writes past the end", &names);
    for name in [MID, EDGE] {
        assert_eq!(
            stats[name],
            stats[&control(name)],
            "the kernel's size and allocation of {name} differ from its control's"
        );
        assert_eq!(
            stats[name].0,
            expected[name].len() as u64,
            "the kernel's size of {name} is not the end of the last write"
        );
        assert_eq!(
            after[name].0,
            sha256_hex(&expected[name]),
            "the kernel reads back different bytes from {name} than were written"
        );
        assert_eq!(
            after[name].0,
            after[&control(name)].0,
            "{name} does not read as the kernel's own writes left its control"
        );
    }
}

/// `far.bin` and `near.bin` are each 40,960 random bytes, checksummed, in
/// one extent; each `*-control.bin` is written by the same `tee`. The
/// kernel cuts its controls to 40,000 bytes and then makes the writes
/// [`a_write_that_starts_past_the_end_zeroes_the_rest_of_the_old_last_sector`]
/// makes through this driver, at the same offsets, in transactions of its
/// own: `far` several sectors past the end, `near` inside the old last
/// sector but past the end.
const GAP_FILL: &str =
    "head -c 40960 /dev/urandom | tee \"$MNT/far.bin\" > \"$MNT/far-control.bin\"\n\
     head -c 40960 /dev/urandom | tee \"$MNT/near.bin\" > \"$MNT/near-control.bin\"\n\
     sync\n\
     truncate -s 40000 \"$MNT/far-control.bin\"\n\
     truncate -s 40000 \"$MNT/near-control.bin\"\n\
     sync\n\
     head -c 100 /dev/zero | tr '\\000' '\\132' \
     | dd of=\"$MNT/far-control.bin\" bs=1 seek=53000 conv=notrunc status=none\n\
     sync\n\
     head -c 50 /dev/zero | tr '\\000' '\\245' \
     | dd of=\"$MNT/near-control.bin\" bs=1 seek=40500 conv=notrunc status=none\n\
     sync";

/// The kernel's `stat` of each named file: its size, and the bytes it
/// says are allocated (`%b` blocks of `%B` bytes).
fn kernel_stats(image: &Path, what: &str, names: &[&str]) -> BTreeMap<String, (u64, u64)> {
    let mut script = String::new();
    for name in names {
        script.push_str(&format!("stat -c '{name} %s %b %B' \"$MNT/{name}\"\n"));
    }
    guest_kernel_read_ok(&image.to_string_lossy(), what, &script)
        .lines()
        .map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(words.len(), 4, "{what}: stat printed {line:?}");
            let n = |w: &str| -> u64 {
                w.parse()
                    .unwrap_or_else(|e| panic!("{what}: stat printed {line:?}: {e}"))
            };
            (
                words[0].to_string(),
                (n(words[1]), n(words[2]) * n(words[3])),
            )
        })
        .collect()
}

/// A write that starts past the end of a file leaves a gap (#433). Before
/// the kernel extends a file whose end lies partway through a sector, it
/// zeroes the rest of that sector, so the bytes between the old end and
/// the next sector boundary read as zeros whatever the extent held there;
/// the gap past that is an implicit hole with no item on a `no-holes`
/// volume, and the write's own sectors are a new extent.
///
/// This driver's truncate keeps an extent's last sector as it was, so
/// `far.bin` and `near.bin`, cut from 40,960 bytes to 40,000, still hold
/// random bytes on disk past their end: the test reads that sector raw
/// and shows them there, so a write that skipped the zeroing would read
/// them back. The kernel made the same writes into the controls, so the
/// expected bytes, `size`, `nbytes` and allocation are its own.
/// `btrfs check --check-data-csum` and a scrub verify every digest.
#[test]
fn a_write_that_starts_past_the_end_zeroes_the_rest_of_the_old_last_sector() {
    const FAR: &str = "far.bin";
    const NEAR: &str = "near.bin";
    const OLD_END: u64 = 40_000;
    const SECTOR: u64 = 4096;
    let control = |name: &str| name.replace(".bin", "-control.bin");
    let scratch = Scratch::new("cow-write-gap", GAP_FILL);
    let image = scratch.image();

    let mut fs = mount_rw(&image);
    for name in [FAR, NEAR] {
        let ino = ino_of(&fs, name);
        let inode = fs.read_inode(ino).unwrap();
        assert_eq!(inode.size, 40_960, "{name} is not the size made");
        assert_eq!(
            inode.flags & fs_btrfs::write::INODE_NODATASUM,
            0,
            "{name} must be checksummed for this to test anything"
        );
        fs.truncate(ino, OLD_END)
            .unwrap_or_else(|e| panic!("cutting {name} to {OLD_END}: {e}"));
    }
    drop(fs);

    // Where the kernel says each file's last sector lies, read raw: the
    // bytes past the old end are the random ones the truncate kept.
    let last_block = OLD_END / SECTOR;
    let mapped = kernel_map(&image, "where the old last sectors lie", &[FAR, NEAR]);
    let mut fs = mount_rw(&image);
    let mut expected: BTreeMap<&str, Vec<u8>> = BTreeMap::new();
    for name in [FAR, NEAR] {
        let extent = mapped[name]
            .1
            .iter()
            .filter(|m| m.logical <= last_block)
            .max_by_key(|m| m.logical)
            .unwrap_or_else(|| panic!("{name}: no extent holds block {last_block}"));
        let logical = (extent.physical + last_block - extent.logical) * SECTOR;
        let at = fs
            .chunk_map()
            .map_mirror(logical, 0)
            .unwrap_or_else(|e| panic!("{name}: mapping {logical}: {e}"))
            .physical;
        let mut raw = vec![0u8; SECTOR as usize];
        {
            use std::io::{Read, Seek, SeekFrom};
            let mut file = std::fs::File::open(&image).unwrap();
            file.seek(SeekFrom::Start(at)).unwrap();
            file.read_exact(&mut raw).unwrap();
        }
        let tail = &raw[(OLD_END % SECTOR) as usize..];
        assert!(
            tail.iter().any(|&b| b != 0),
            "{name}: the bytes on disk past the old end are already zeros, so this tests nothing"
        );
        let file = fs.read_file(ino_of(&fs, name)).unwrap();
        assert_eq!(file.len() as u64, OLD_END, "{name} was not cut");
        expected.insert(name, file);
    }

    let writes: Vec<(&str, u64, Vec<u8>)> = vec![
        (FAR, 53_000, vec![0x5a; 100]),
        (NEAR, 40_500, vec![0xa5; 50]),
    ];
    let generation = fs.superblock().generation;
    for (name, offset, bytes) in &writes {
        let ino = ino_of(&fs, name);
        let n = fs.write(ino, *offset, bytes).unwrap_or_else(|e| {
            panic!(
                "writing {} bytes at {offset} of {name}, past its end at {OLD_END}: {e}",
                bytes.len()
            )
        });
        assert_eq!(n, bytes.len(), "a short write");
        let file = expected.get_mut(name).unwrap();
        let end = *offset as usize + bytes.len();
        file.resize(end, 0);
        file[*offset as usize..end].copy_from_slice(bytes);
        assert_eq!(
            fs.read_inode(ino).unwrap().size,
            end as u64,
            "{name}'s size is not the end of the write"
        );
        // Our own reading of what we committed, before the oracles'.
        assert_eq!(
            &fs.read_file(ino).unwrap(),
            file,
            "{name} reads back differently through this driver after the write"
        );
    }
    assert_eq!(
        fs.superblock().generation,
        generation + writes.len() as u64,
        "each write is one committed transaction"
    );
    for name in [FAR, NEAR] {
        let ours = fs.read_inode(ino_of(&fs, name)).unwrap();
        let theirs = fs.read_inode(ino_of(&fs, &control(name))).unwrap();
        assert_eq!(
            (ours.size, ours.nbytes),
            (theirs.size, theirs.nbytes),
            "{name}'s size and nbytes are not what the kernel's own writes gave its control"
        );
    }
    drop(fs);

    assert_btrfs_check_clean(&image, "after writes that leave a gap");
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum finds data that disagrees with its digests:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub after writes that leave a gap",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );

    let names = [FAR, NEAR, "far-control.bin", "near-control.bin"];
    let stats = kernel_stats(
        &image,
        "the kernel's stat after writes that leave a gap",
        &names,
    );
    let after = kernel_view(&image, "after writes that leave a gap", &names);
    for name in [FAR, NEAR] {
        assert_eq!(
            stats[name],
            stats[&control(name)],
            "the kernel's size and allocation of {name} differ from its control's"
        );
        assert_eq!(
            after[name].0,
            sha256_hex(&expected[name]),
            "the kernel reads back different bytes from {name} than were written: the gap \
             and the old last sector's tail must read as zeros"
        );
        assert_eq!(
            after[name].0,
            after[&control(name)].0,
            "{name} does not read as the kernel's own writes left its control"
        );
    }
}

/// Two checksummed files of 40,960 random bytes, each written twice by the
/// kernel: the copies are the controls, which the kernel cuts to 40,000
/// bytes and then grows by truncate to the sizes
/// [`a_truncate_that_grows_a_file_cut_partway_through_a_sector_reads_zeros_past_the_old_end`]
/// grows the originals to through this driver: `far` several sectors past
/// the old end, `near` inside the old last sector.
const CUT_GROW_FILL: &str =
    "head -c 40960 /dev/urandom | tee \"$MNT/far.bin\" > \"$MNT/far-control.bin\"\n\
     head -c 40960 /dev/urandom | tee \"$MNT/near.bin\" > \"$MNT/near-control.bin\"\n\
     sync\n\
     truncate -s 40000 \"$MNT/far-control.bin\"\n\
     truncate -s 40000 \"$MNT/near-control.bin\"\n\
     sync\n\
     truncate -s 53100 \"$MNT/far-control.bin\"\n\
     truncate -s 40550 \"$MNT/near-control.bin\"\n\
     sync";

/// A truncate that grows a file is the other way, beside a write past the
/// end (#433), to expose the rest of a sector the file once filled (#524).
/// This driver's truncate keeps an extent's last sector as it was, so
/// `far.bin` and `near.bin`, cut from 40,960 bytes to 40,000, still hold
/// random bytes on disk past their end: the test reads that sector raw and
/// shows them there. Each is then grown by truncate past the old end, and
/// the bytes from the old end onward must read as zeros through the
/// kernel, as they do in the controls the kernel cut and grew the same way.
/// `btrfs check --check-data-csum` and a scrub verify every digest.
#[test]
fn a_truncate_that_grows_a_file_cut_partway_through_a_sector_reads_zeros_past_the_old_end() {
    const FAR: &str = "far.bin";
    const NEAR: &str = "near.bin";
    const OLD_END: u64 = 40_000;
    const SECTOR: u64 = 4096;
    let control = |name: &str| name.replace(".bin", "-control.bin");
    let scratch = Scratch::new("truncate-grow-after-cut", CUT_GROW_FILL);
    let image = scratch.image();

    let mut fs = mount_rw(&image);
    for name in [FAR, NEAR] {
        let ino = ino_of(&fs, name);
        let inode = fs.read_inode(ino).unwrap();
        assert_eq!(inode.size, 40_960, "{name} is not the size made");
        assert_eq!(
            inode.flags & fs_btrfs::write::INODE_NODATASUM,
            0,
            "{name} must be checksummed for this to test anything"
        );
        fs.truncate(ino, OLD_END)
            .unwrap_or_else(|e| panic!("cutting {name} to {OLD_END}: {e}"));
    }
    drop(fs);

    // Where the kernel says each file's last sector lies, read raw: the
    // bytes past the old end are the random ones the truncate kept.
    let last_block = OLD_END / SECTOR;
    let mapped = kernel_map(&image, "where the old last sectors lie", &[FAR, NEAR]);
    let mut fs = mount_rw(&image);
    let mut expected: BTreeMap<&str, Vec<u8>> = BTreeMap::new();
    for name in [FAR, NEAR] {
        let extent = mapped[name]
            .1
            .iter()
            .filter(|m| m.logical <= last_block)
            .max_by_key(|m| m.logical)
            .unwrap_or_else(|| panic!("{name}: no extent holds block {last_block}"));
        let logical = (extent.physical + last_block - extent.logical) * SECTOR;
        let at = fs
            .chunk_map()
            .map_mirror(logical, 0)
            .unwrap_or_else(|e| panic!("{name}: mapping {logical}: {e}"))
            .physical;
        let mut raw = vec![0u8; SECTOR as usize];
        {
            use std::io::{Read, Seek, SeekFrom};
            let mut file = std::fs::File::open(&image).unwrap();
            file.seek(SeekFrom::Start(at)).unwrap();
            file.read_exact(&mut raw).unwrap();
        }
        let tail = &raw[(OLD_END % SECTOR) as usize..];
        assert!(
            tail.iter().any(|&b| b != 0),
            "{name}: the bytes on disk past the old end are already zeros, so this tests nothing"
        );
        let file = fs.read_file(ino_of(&fs, name)).unwrap();
        assert_eq!(file.len() as u64, OLD_END, "{name} was not cut");
        expected.insert(name, file);
    }

    for (name, size) in [(FAR, 53_100u64), (NEAR, 40_550)] {
        let ino = ino_of(&fs, name);
        fs.truncate(ino, size)
            .unwrap_or_else(|e| panic!("growing {name} from {OLD_END} to {size}: {e}"));
        let file = expected.get_mut(name).unwrap();
        file.resize(size as usize, 0);
        assert_eq!(
            fs.read_inode(ino).unwrap().size,
            size,
            "{name}'s size is not the size it was grown to"
        );
        // Our own reading of what we committed, before the oracles'.
        assert_eq!(
            &fs.read_file(ino).unwrap(),
            file,
            "{name} reads back differently through this driver after it was grown"
        );
    }
    drop(fs);

    assert_btrfs_check_clean(&image, "after truncates that grow cut files");
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum finds data that disagrees with its digests:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub after truncates that grow cut files",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );

    let names = [FAR, NEAR, "far-control.bin", "near-control.bin"];
    let stats = kernel_stats(
        &image,
        "the kernel's stat after truncates that grow cut files",
        &names,
    );
    let after = kernel_view(&image, "after truncates that grow cut files", &names);
    for name in [FAR, NEAR] {
        assert_eq!(
            stats[name].0,
            stats[&control(name)].0,
            "the kernel's size of {name} differs from its control's"
        );
        assert_eq!(
            after[name].0,
            sha256_hex(&expected[name]),
            "the kernel reads back different bytes from {name} than it was cut to and then \
             zeros: the old last sector's tail must read as zeros"
        );
        assert_eq!(
            after[name].0,
            after[&control(name)].0,
            "{name} does not read as the kernel's own cut and growth left its control"
        );
    }
}

/// Without the no-holes feature a gap is an item of its own (#436). The
/// kernel extends a file a write starts past the end of by inserting a
/// hole item from the sector after the old end to the sector the write
/// starts in, and an append that starts at the end leaves no hole at all.
/// `app.bin` is appended to at its end and `far.bin` written several
/// sectors past it, both checksummed and ending partway through a sector;
/// the kernel made the same writes into the controls, so the hole items,
/// `size`, `nbytes` and bytes the driver is held to are its own.
/// `btrfs check --check-data-csum` and a scrub verify every digest, and
/// the kernel reads every byte back.
#[test]
fn a_write_that_grows_a_file_without_no_holes_records_the_gap_as_a_hole_item() {
    const APP: &str = "app.bin";
    const FAR: &str = "far.bin";
    let control = |name: &str| name.replace(".bin", "-control.bin");
    let writes = [
        (APP, 40_000u64, 9000usize, 0xc3u8),
        (FAR, 70_000, 5000, 0x3c),
    ];
    let mut script = String::new();
    for name in [APP, FAR] {
        script.push_str(&format!(
            "head -c 40000 /dev/urandom | tee \"$MNT/{name}\" > \"$MNT/{}\"\n",
            control(name)
        ));
    }
    script.push_str("sync\n");
    for (name, offset, len, byte) in writes {
        script.push_str(&format!(
            "head -c {len} /dev/zero | tr '\\000' '\\{byte:03o}' \
             | dd of=\"$MNT/{}\" bs=1 seek={offset} conv=notrunc status=none\n\
             sync\n",
            control(name)
        ));
    }
    let scratch = Scratch::made("cow-write-grow-holes", "crc32c", "^no-holes", &script);
    let image = scratch.image();
    let mut fs = mount_rw(&image);
    assert!(!fs.superblock().has_no_holes(), "mkfs enabled NO_HOLES");

    let mut expected = BTreeMap::new();
    for (name, offset, len, byte) in writes {
        let ino = ino_of(&fs, name);
        let mut bytes = fs.read_file(ino).unwrap();
        assert_eq!(bytes.len(), 40_000, "{name} before the write");
        assert_eq!(
            fs.write(ino, offset, &vec![byte; len])
                .unwrap_or_else(|e| panic!("growing {name} with a write at {offset}: {e}")),
            len
        );
        bytes.resize(offset as usize, 0);
        bytes.extend(std::iter::repeat_n(byte, len));
        assert_eq!(fs.read_file(ino).unwrap(), bytes, "{name} reads back");
        expected.insert(name, bytes);
    }
    for (name, offset, len, _) in writes {
        let ours = fs.read_inode(ino_of(&fs, name)).unwrap();
        let theirs = fs.read_inode(ino_of(&fs, &control(name))).unwrap();
        assert_eq!(
            (ours.size, ours.nbytes),
            (theirs.size, theirs.nbytes),
            "{name}'s size and nbytes are not what the kernel's own writes gave its control"
        );
        let to = (offset + len as u64).next_multiple_of(4096);
        assert_eq!(
            explicit_holes_to(&image, ino_of(&fs, name), to),
            explicit_holes_to(&image, ino_of(&fs, &control(name)), to),
            "{name}: hole items differ from the kernel's control"
        );
    }
    assert_eq!(
        explicit_holes_to(&image, ino_of(&fs, &control(FAR)), 77_824),
        vec![(40_960, 28_672)],
        "the kernel must have recorded the gap as one hole item"
    );
    drop(fs);

    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum after writes that grow a file without no-holes:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub after writes that grow a file without no-holes",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );
    let names = [APP, FAR, "app-control.bin", "far-control.bin"];
    let seen = kernel_view(
        &image,
        "after writes that grow a file without no-holes",
        &names,
    );
    for name in [APP, FAR] {
        assert_eq!(
            seen[name].0,
            sha256_hex(&expected[name]),
            "{name}: wrong bytes"
        );
        assert_eq!(
            seen[name].0,
            seen[&control(name)].0,
            "{name} does not read as the kernel's own writes left its control"
        );
    }
}

/// A new gap adjoining an existing trailing hole extends that hole's item
/// (#602, part of #526), including an EOF partway through a hole sector.
/// The kernel writes identical controls; its item coverage, size, nbytes,
/// byte readback and scrub are independent of this crate's interpretation.
#[test]
fn a_growing_write_coalesces_the_gap_with_the_trailing_hole() {
    let cases = [
        ("aligned", 32_768, 8192),
        ("partial", 40_000, 8192),
        ("sparse", 40_000, 0),
    ];
    let mut script = String::new();
    for (name, size, data_len) in cases {
        script.push_str(&format!(
            "head -c {data_len} /dev/urandom | tee \"$MNT/{name}\" > \"$MNT/{name}-control\"\n\
             truncate -s {size} \"$MNT/{name}\" \"$MNT/{name}-control\"\n"
        ));
    }
    script.push_str("sync\n");
    for (name, _, _) in cases {
        script.push_str(&format!(
            "head -c 5000 /dev/zero | tr '\\000' '\\303' \
             | dd of=\"$MNT/{name}-control\" bs=1 seek=65536 conv=notrunc status=none\n\
             sync\n"
        ));
    }
    let scratch = Scratch::made("cow-grow-trailing-hole", "crc32c", "^no-holes", &script);
    let image = scratch.image();
    let mut fs = mount_rw(&image);
    assert!(!fs.superblock().has_no_holes(), "mkfs enabled NO_HOLES");
    let mut expected = BTreeMap::new();
    for (name, size, data_len) in cases {
        let ino = ino_of(&fs, name);
        let inode = fs.read_inode(ino).unwrap();
        assert_eq!((inode.size, inode.nbytes), (size, data_len));
        assert_eq!(
            explicit_holes_to(&image, ino, size.next_multiple_of(4096)),
            vec![(data_len, size.next_multiple_of(4096) - data_len)],
            "{name}: the kernel must have made one trailing hole"
        );
        let mut bytes = fs.read_file(ino).unwrap();
        bytes.resize(65_536, 0);
        bytes.extend_from_slice(&[0xc3; 5000]);
        assert_eq!(fs.write(ino, 65_536, &[0xc3; 5000]).unwrap(), 5000);
        assert_eq!(fs.read_file(ino).unwrap(), bytes, "{name}");
        expected.insert(name, bytes);
        let ours = fs.read_inode(ino).unwrap();
        let control = ino_of(&fs, &format!("{name}-control"));
        let theirs = fs.read_inode(control).unwrap();
        assert_eq!(
            (ours.size, ours.nbytes),
            (theirs.size, theirs.nbytes),
            "{name}"
        );
        let kernel_holes = explicit_holes_to(&image, control, 73_728);
        assert_eq!(kernel_holes, vec![(data_len, 65_536 - data_len)]);
        assert_eq!(
            explicit_holes_to(&image, ino, 73_728),
            kernel_holes,
            "{name}: the old hole and the new gap differ from the kernel's items"
        );
    }
    drop(fs);
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum after trailing-hole growth:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub after trailing-hole growth",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );
    let names = [
        "aligned",
        "aligned-control",
        "partial",
        "partial-control",
        "sparse",
        "sparse-control",
    ];
    let seen = kernel_view(&image, "read back trailing-hole growth", &names);
    for (name, _, _) in cases {
        assert_eq!(
            seen[name].0,
            sha256_hex(&expected[name]),
            "{name}: wrong bytes"
        );
        assert_eq!(
            seen[name].0,
            seen[&format!("{name}-control")].0,
            "{name}: differs from kernel control"
        );
    }
}

/// A hole at the old EOF needs no zeroing allocation (#601). The kernel
/// independently grows twin files from a partial hole sector: into the
/// next sector, several sectors away, and inside the old EOF sector
/// itself (where only the sector the payload touches must be allocated).
/// Compare every extent item's span and kind, inode size and nbytes,
/// exact expected content hashes, data checksums and a kernel scrub.
#[test]
fn a_growing_write_keeps_an_untouched_old_eof_hole_sector_sparse() {
    const OLD_SIZE: u64 = 40_000;
    const SECTOR: u64 = 4096;
    let cases = [
        ("far", 8192u64, 70_000u64, 5000usize),
        ("next", 8192, 40_960, 100),
        ("same", 8192, 40_100, 100),
        ("empty", 0, 70_000, 5000),
    ];
    let mut script = String::new();
    for (name, data_len, _, _) in cases {
        script.push_str(&format!(
            "head -c {data_len} /dev/urandom | tee \"$MNT/{name}\" > \"$MNT/{name}-control\"\n\
             truncate -s {OLD_SIZE} \"$MNT/{name}\" \"$MNT/{name}-control\"\n"
        ));
    }
    script.push_str("sync\n");
    for (name, _, offset, len) in cases {
        script.push_str(&format!(
            "head -c {len} /dev/zero | tr '\\000' '\\245' \
             | dd of=\"$MNT/{name}-control\" bs=1 seek={offset} conv=notrunc status=none\n\
             sync\n"
        ));
    }
    let scratch = Scratch::made("cow-sparse-eof", "crc32c", "^no-holes", &script);
    let image = scratch.image();
    let mut fs = mount_rw(&image);
    assert!(!fs.superblock().has_no_holes(), "mkfs enabled NO_HOLES");
    let mut expected = BTreeMap::new();
    for (name, data_len, offset, len) in cases {
        let ino = ino_of(&fs, name);
        let before = fs.read_inode(ino).unwrap();
        assert_eq!((before.size, before.nbytes), (OLD_SIZE, data_len));
        assert_eq!(
            before.flags & (fs_btrfs::write::INODE_NODATACOW | fs_btrfs::write::INODE_NODATASUM),
            0,
            "{name}: the kernel must have made a checksummed CoW file"
        );
        assert_eq!(
            explicit_holes_to(&image, ino, OLD_SIZE.next_multiple_of(SECTOR)),
            vec![(data_len, OLD_SIZE.next_multiple_of(SECTOR) - data_len)],
            "{name}: old EOF must lie inside an explicit hole"
        );
        let mut bytes = fs.read_file(ino).unwrap();
        bytes.resize(offset as usize, 0);
        bytes.extend(std::iter::repeat_n(0xa5, len));
        assert_eq!(fs.write(ino, offset, &vec![0xa5; len]).unwrap(), len);
        assert_eq!(fs.read_file(ino).unwrap(), bytes, "{name}");
        expected.insert(name, bytes);

        let ours = fs.read_inode(ino).unwrap();
        let control = ino_of(&fs, &format!("{name}-control"));
        let theirs = fs.read_inode(control).unwrap();
        let start = offset / SECTOR * SECTOR;
        let end = (offset + len as u64).next_multiple_of(SECTOR);
        assert_eq!(
            (ours.size, ours.nbytes),
            (theirs.size, theirs.nbytes),
            "{name}: allocation differs from the kernel's write"
        );
        assert_eq!(
            ours.nbytes - before.nbytes,
            end - start,
            "{name}: only payload sectors may be allocated"
        );
        let layout = vec![
            (data_len, start - data_len, true),
            (start, end - start, false),
        ];
        let mut wanted = Vec::new();
        if data_len > 0 {
            wanted.push((0, data_len, false));
        }
        wanted.extend(layout);
        let kernel_layout = extent_layout_to(&image, control, end);
        assert_eq!(kernel_layout, wanted, "{name}: kernel extent items");
        assert_eq!(
            extent_layout_to(&image, ino, end),
            kernel_layout,
            "{name}: extent items differ from the kernel's write"
        );
    }
    drop(fs);
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum after sparse EOF growth:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub after sparse EOF growth",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );
    let names = [
        "far",
        "far-control",
        "next",
        "next-control",
        "same",
        "same-control",
        "empty",
        "empty-control",
    ];
    let seen = kernel_view(&image, "read back sparse EOF growth", &names);
    let stats = kernel_stats(&image, "stat sparse EOF growth", &names);
    for (name, _, _, _) in cases {
        let control = format!("{name}-control");
        assert_eq!(
            seen[name].0,
            sha256_hex(&expected[name]),
            "{name}: wrong bytes"
        );
        assert_eq!(
            seen[name].0, seen[&control].0,
            "{name}: kernel control differs"
        );
        assert_eq!(
            stats[name], stats[&control],
            "{name}: kernel size and blocks"
        );
    }
}

/// How `btrfs inspect-internal dump-tree` shows inode `ino`'s inline
/// extent at offset zero: its data size, `ram_bytes` and compression.
/// `None` when the item is not inline.
fn inline_extent(image: &Path, ino: u64) -> Option<(u64, u64, String)> {
    let tree = dump_tree(image, "5");
    let lines: Vec<&str> = tree.lines().collect();
    let needle = format!("key ({ino} EXTENT_DATA 0)");
    let at = lines
        .iter()
        .position(|l| l.contains(&needle))
        .unwrap_or_else(|| panic!("inode {ino} has no extent item at 0\n{tree}"));
    let line = lines[at + 1..]
        .iter()
        .take_while(|l| !l.contains(" key ("))
        .find(|l| l.contains("inline extent data size"))?;
    let words: Vec<&str> = line.split_whitespace().collect();
    let after = |word: &str| -> &str {
        let i = words
            .iter()
            .position(|w| *w == word)
            .unwrap_or_else(|| panic!("no {word} in {line}"));
        words[i + 1]
    };
    Some((
        after("size").parse().unwrap(),
        after("ram_bytes").parse().unwrap(),
        after("compression").to_string(),
    ))
}

/// A small file's data is inline, held in its `EXTENT_DATA` item (#437).
/// A write that lies within the file's size rewrites that item with the
/// new bytes, its size unchanged, and the file stays inline: no data
/// extent, extent record, free space or checksum moves. Each file here is
/// below the mount's `max_inline`, and is written at its start, in its
/// middle and up to its last byte, one transaction per write; the kernel
/// made the same writes into the controls, so the bytes, `size`, `nbytes`
/// and the inline item's `ram_bytes` the driver is held to are its own.
#[test]
fn a_write_inside_an_inline_extent_rewrites_it_and_the_file_stays_inline() {
    let control = |name: &str| name.replace(".bin", "-control.bin");
    let files = [("small.bin", 300u64), ("mid.bin", 1000), ("big.bin", 2000)];
    // (file, offset, length, byte): the start, the middle, the last byte.
    let writes = [
        ("small.bin", 0u64, 17usize, 0x5au8),
        ("small.bin", 120, 40, 0xa5),
        ("small.bin", 290, 10, 0x11),
        ("mid.bin", 0, 1, 0x22),
        ("mid.bin", 500, 333, 0x33),
        ("mid.bin", 999, 1, 0x44),
        ("big.bin", 0, 2000, 0x55),
        ("big.bin", 1024, 100, 0x66),
        ("big.bin", 1900, 100, 0x77),
    ];
    let mut script = String::new();
    for (name, size) in files {
        script.push_str(&format!(
            "head -c {size} /dev/urandom | tee \"$MNT/{name}\" > \"$MNT/{}\"\n",
            control(name)
        ));
    }
    script.push_str("sync\n");
    for (name, offset, len, byte) in writes {
        script.push_str(&format!(
            "head -c {len} /dev/zero | tr '\\000' '\\{byte:03o}' \
             | dd of=\"$MNT/{}\" bs=1 seek={offset} conv=notrunc status=none\n\
             sync\n",
            control(name)
        ));
    }
    let scratch = Scratch::new("cow-write-inline", &script);
    let image = scratch.image();
    for (name, size) in files {
        let fs = mount_rw(&image);
        let (ino, theirs) = (ino_of(&fs, name), ino_of(&fs, &control(name)));
        drop(fs);
        let inline = inline_extent(&image, ino)
            .unwrap_or_else(|| panic!("the kernel did not store {name} inline"));
        assert_eq!(inline, (size, size, "0".to_string()), "{name} before");
        assert!(
            inline_extent(&image, theirs).is_some(),
            "the kernel's writes moved {}'s data out of line",
            control(name)
        );
    }

    let mut fs = mount_rw(&image);
    let mut expected = BTreeMap::new();
    for (name, _) in files {
        let ino = ino_of(&fs, name);
        expected.insert(name, fs.read_file(ino).unwrap());
    }
    for (name, offset, len, byte) in writes {
        let ino = ino_of(&fs, name);
        assert_eq!(
            fs.write(ino, offset, &vec![byte; len])
                .unwrap_or_else(|e| panic!("writing {name} at {offset}: {e}")),
            len
        );
        let bytes = expected.get_mut(name).unwrap();
        bytes[offset as usize..offset as usize + len].fill(byte);
        assert_eq!(&fs.read_file(ino).unwrap(), bytes, "{name} reads back");
    }
    for (name, _) in files {
        let ours = fs.read_inode(ino_of(&fs, name)).unwrap();
        let theirs = fs.read_inode(ino_of(&fs, &control(name))).unwrap();
        assert_eq!(
            (ours.size, ours.nbytes),
            (theirs.size, theirs.nbytes),
            "{name}'s size and nbytes are not what the kernel's own writes gave its control"
        );
    }
    let inos: Vec<(&str, u64, u64)> = files
        .iter()
        .map(|&(name, _)| (name, ino_of(&fs, name), ino_of(&fs, &control(name))))
        .collect();
    drop(fs);
    for (name, ours, theirs) in inos {
        let theirs = inline_extent(&image, theirs)
            .unwrap_or_else(|| panic!("{} is not inline", control(name)));
        assert_eq!(
            inline_extent(&image, ours),
            Some(theirs),
            "{name} is not inline as its control is"
        );
    }

    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum after writes inside inline extents:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    let names: Vec<String> = files
        .iter()
        .flat_map(|&(name, _)| [name.to_string(), control(name)])
        .collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let seen = kernel_view(&image, "after writes inside inline extents", &refs);
    for (name, _) in files {
        assert_eq!(
            seen[name].0,
            sha256_hex(&expected[name]),
            "{name}: wrong bytes"
        );
        assert_eq!(
            seen[name].0,
            seen[&control(name)].0,
            "{name} does not read as the kernel's own writes left its control"
        );
    }
}

/// A small compressible file written through a `compress=zlib` mount, and
/// another through `compress=zstd`, are stored as compressed inline
/// extents (#438). A write inside each decodes the item, applies the
/// bytes and stores the result inline again, compressed with the mount's
/// algorithm when that is smaller, as the kernel decides. The kernel made
/// the same writes into the controls, so the bytes, `size`, `nbytes` and
/// the inline item's `ram_bytes` the driver is held to are its own.
#[test]
fn a_write_inside_a_compressed_inline_extent_rewrites_it_and_the_file_stays_inline() {
    use fs_btrfs::compression::Compression;
    let control = |name: &str| format!("{name}-control");
    // (file, mount algorithm, size).
    let files = [
        ("zl", "zlib", Compression::Zlib, 3000u64),
        ("zs", "zstd", Compression::Zstd, 2500),
    ];
    // (file, offset, length, byte): the start, the middle, the last byte.
    let writes = [
        ("zl", 0u64, 17usize, 0x5au8),
        ("zl", 1200, 400, 0xa5),
        ("zl", 2999, 1, 0x11),
        ("zs", 0, 1, 0x22),
        ("zs", 700, 333, 0x33),
        ("zs", 2400, 100, 0x44),
    ];
    let mut script = String::new();
    for (name, mount, _, size) in files {
        script.push_str(&format!(
            "mount -o remount,compress={mount} \"$MNT\"\n\
             awk -v size={size} 'BEGIN {{ s = \"a compressible line of text\\n\"; \
               while (length(s) < size) s = s s; printf \"%s\", substr(s, 1, size) }}' \
               | tee \"$MNT/{name}\" \
             > \"$MNT/{}\"\n\
             sync\n",
            control(name)
        ));
        for (file, offset, len, byte) in writes {
            if file != name {
                continue;
            }
            script.push_str(&format!(
                "head -c {len} /dev/zero | tr '\\000' '\\{byte:03o}' \
                 | dd of=\"$MNT/{}\" bs=1 seek={offset} conv=notrunc status=none\n\
                 sync\n",
                control(name)
            ));
        }
    }
    let scratch = Scratch::new("cow-write-compressed-inline", &script);
    let image = scratch.image();
    for (name, _, _, size) in files {
        let fs = mount_rw(&image);
        let (ino, theirs) = (ino_of(&fs, name), ino_of(&fs, &control(name)));
        drop(fs);
        let (_, ram, compression) = inline_extent(&image, ino)
            .unwrap_or_else(|| panic!("the kernel did not store {name} inline"));
        assert_eq!(ram, size, "{name}'s inline ram_bytes before");
        assert_ne!(compression, "0", "the kernel did not compress {name}");
        assert!(
            inline_extent(&image, theirs).is_some(),
            "the kernel's writes moved {}'s data out of line",
            control(name)
        );
    }

    let mut fs = mount_rw(&image);
    let mut expected = BTreeMap::new();
    for (name, _, _, _) in files {
        let ino = ino_of(&fs, name);
        expected.insert(name, fs.read_file(ino).unwrap());
    }
    for (name, _, algo, _) in files {
        fs.set_compression(algo);
        for (file, offset, len, byte) in writes {
            if file != name {
                continue;
            }
            let ino = ino_of(&fs, name);
            assert_eq!(
                fs.write(ino, offset, &vec![byte; len])
                    .unwrap_or_else(|e| panic!("writing {name} at {offset}: {e}")),
                len
            );
            let bytes = expected.get_mut(name).unwrap();
            bytes[offset as usize..offset as usize + len].fill(byte);
            assert_eq!(&fs.read_file(ino).unwrap(), bytes, "{name} reads back");
        }
    }
    for (name, _, _, _) in files {
        let ours = fs.read_inode(ino_of(&fs, name)).unwrap();
        let theirs = fs.read_inode(ino_of(&fs, &control(name))).unwrap();
        assert_eq!(
            (ours.size, ours.nbytes),
            (theirs.size, theirs.nbytes),
            "{name}'s size and nbytes are not what the kernel's own writes gave its control"
        );
    }
    let inos: Vec<(&str, u64, u64)> = files
        .iter()
        .map(|&(name, _, _, _)| (name, ino_of(&fs, name), ino_of(&fs, &control(name))))
        .collect();
    drop(fs);
    for (name, ours, theirs) in inos {
        let (_, their_ram, _) = inline_extent(&image, theirs)
            .unwrap_or_else(|| panic!("{} is not inline", control(name)));
        let (_, our_ram, _) =
            inline_extent(&image, ours).unwrap_or_else(|| panic!("{name} is not inline"));
        assert_eq!(our_ram, their_ram, "{name}'s inline ram_bytes");
    }

    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum after writes inside compressed inline extents:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    let names: Vec<String> = files
        .iter()
        .flat_map(|&(name, _, _, _)| [name.to_string(), control(name)])
        .collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let seen = kernel_view(
        &image,
        "after writes inside compressed inline extents",
        &refs,
    );
    for (name, _, _, _) in files {
        assert_eq!(
            seen[name].0,
            sha256_hex(&expected[name]),
            "{name}: wrong bytes"
        );
        assert_eq!(
            seen[name].0,
            seen[&control(name)].0,
            "{name} does not read as the kernel's own writes left its control"
        );
    }
}

/// A write that grows a file whose data is all inline, and whose grown
/// data still fits inline — short of one sector and within the mount's
/// `max_inline` — rewrites the inline item, longer, and moves the
/// inode's `size` and `nbytes`; the file stays inline, as the kernel
/// leaves it (#530). Each file is grown once from inside its data past
/// its end and once from its end, one transaction per write; the kernel
/// made the same writes into the controls, so the bytes, `size`, `nbytes`
/// and the inline item's `ram_bytes` the driver is held to are its own.
#[test]
fn a_write_growing_an_inline_file_within_the_inline_limit_keeps_it_inline() {
    let control = |name: &str| name.replace(".bin", "-control.bin");
    let files = [("small.bin", 300u64), ("mid.bin", 1000)];
    // (file, offset, length, byte): across the end, then from the end.
    let writes = [
        ("small.bin", 250u64, 100usize, 0x5au8),
        ("small.bin", 350, 200, 0xa5),
        ("mid.bin", 900, 300, 0x33),
        ("mid.bin", 1200, 800, 0x44),
    ];
    let mut script = String::new();
    for (name, size) in files {
        script.push_str(&format!(
            "head -c {size} /dev/urandom | tee \"$MNT/{name}\" > \"$MNT/{}\"\n",
            control(name)
        ));
    }
    script.push_str("sync\n");
    for (name, offset, len, byte) in writes {
        script.push_str(&format!(
            "head -c {len} /dev/zero | tr '\\000' '\\{byte:03o}' \
             | dd of=\"$MNT/{}\" bs=1 seek={offset} conv=notrunc status=none\n\
             sync\n",
            control(name)
        ));
    }
    let scratch = Scratch::new("cow-write-grow-inline", &script);
    let image = scratch.image();
    for (name, size) in files {
        let fs = mount_rw(&image);
        let (ino, theirs) = (ino_of(&fs, name), ino_of(&fs, &control(name)));
        drop(fs);
        let inline = inline_extent(&image, ino)
            .unwrap_or_else(|| panic!("the kernel did not store {name} inline"));
        assert_eq!(inline, (size, size, "0".to_string()), "{name} before");
        assert!(
            inline_extent(&image, theirs).is_some(),
            "the kernel's growing writes moved {}'s data out of line",
            control(name)
        );
    }

    let mut fs = mount_rw(&image);
    let mut expected = BTreeMap::new();
    for (name, _) in files {
        let ino = ino_of(&fs, name);
        expected.insert(name, fs.read_file(ino).unwrap());
    }
    for (name, offset, len, byte) in writes {
        let ino = ino_of(&fs, name);
        assert_eq!(
            fs.write(ino, offset, &vec![byte; len])
                .unwrap_or_else(|e| panic!("growing {name} at {offset}: {e}")),
            len
        );
        let bytes = expected.get_mut(name).unwrap();
        bytes.resize(bytes.len().max(offset as usize + len), 0);
        bytes[offset as usize..offset as usize + len].fill(byte);
        assert_eq!(&fs.read_file(ino).unwrap(), bytes, "{name} reads back");
    }
    for (name, _) in files {
        let ours = fs.read_inode(ino_of(&fs, name)).unwrap();
        let theirs = fs.read_inode(ino_of(&fs, &control(name))).unwrap();
        assert_eq!(
            (ours.size, ours.nbytes),
            (theirs.size, theirs.nbytes),
            "{name}'s size and nbytes are not what the kernel's own writes gave its control"
        );
    }
    let inos: Vec<(&str, u64, u64)> = files
        .iter()
        .map(|&(name, _)| (name, ino_of(&fs, name), ino_of(&fs, &control(name))))
        .collect();
    drop(fs);
    for (name, ours, theirs) in inos {
        let theirs = inline_extent(&image, theirs)
            .unwrap_or_else(|| panic!("{} is not inline", control(name)));
        assert_eq!(
            inline_extent(&image, ours),
            Some(theirs),
            "{name} is not inline as its control is"
        );
    }

    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum after growing inline files:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    let names: Vec<String> = files
        .iter()
        .flat_map(|&(name, _)| [name.to_string(), control(name)])
        .collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let seen = kernel_view(&image, "after growing inline files", &refs);
    for (name, _) in files {
        assert_eq!(
            seen[name].0,
            sha256_hex(&expected[name]),
            "{name}: wrong bytes"
        );
        assert_eq!(
            seen[name].0,
            seen[&control(name)].0,
            "{name} does not read as the kernel's own writes left its control"
        );
    }
}

/// The offsets of inode `ino`'s `EXTENT_DATA` items, and whether any of
/// them holds inline data, as `btrfs inspect-internal dump-tree` prints them.
fn extent_offsets(image: &Path, ino: u64) -> (Vec<u64>, bool) {
    let tree = dump_tree(image, "5");
    let lines: Vec<&str> = tree.lines().collect();
    let needle = format!("key ({ino} EXTENT_DATA ");
    let mut offsets = Vec::new();
    let mut inline = false;
    for (at, line) in lines.iter().enumerate() {
        let Some(open) = line.find(&needle) else {
            continue;
        };
        let rest = &line[open + needle.len()..];
        offsets.push(rest[..rest.find(')').unwrap()].parse().unwrap());
        inline |= lines[at + 1..]
            .iter()
            .take_while(|l| !l.contains(" key ("))
            .any(|l| l.contains("inline extent data size"));
    }
    (offsets, inline)
}

/// A write that grows a file whose data is inline past what an inline
/// extent holds moves that data into a regular extent at offset 0, as the
/// kernel does, rather than planning a new extent at the old, unaligned end
/// of the file (#507). The guest kernel writes a 100-byte and a 700-byte
/// inline file and makes the same writes into controls: an append from the
/// end, and a write that starts past the end within the first sector. The
/// driver's files must then hold no inline item and only sector-aligned
/// extents, with the controls' `size` and `nbytes`, and `btrfs check
/// --check-data-csum`, a kernel scrub and a kernel read-back must agree (#531).
#[test]
fn an_append_outgrowing_inline_data_moves_it_into_sector_aligned_extents() {
    let control = |name: &str| name.replace(".bin", "-control.bin");
    let files = [("hundred.bin", 100u64), ("seven.bin", 700)];
    // (file, offset, length, byte): from the end, then past it.
    let writes = [
        ("hundred.bin", 100u64, 5000usize, 0x5au8),
        ("seven.bin", 1500, 3000, 0xa5),
    ];
    let mut script = String::new();
    for (name, size) in files {
        script.push_str(&format!(
            "head -c {size} /dev/urandom | tee \"$MNT/{name}\" > \"$MNT/{}\"\n",
            control(name)
        ));
    }
    script.push_str("sync\n");
    for (name, offset, len, byte) in writes {
        script.push_str(&format!(
            "head -c {len} /dev/zero | tr '\\000' '\\{byte:03o}' \
             | dd of=\"$MNT/{}\" bs=1 seek={offset} conv=notrunc status=none\n\
             sync\n",
            control(name)
        ));
    }
    let scratch = Scratch::new("cow-write-append-inline", &script);
    let image = scratch.image();
    for (name, size) in files {
        let fs = mount_rw(&image);
        let ino = ino_of(&fs, name);
        drop(fs);
        assert_eq!(
            inline_extent(&image, ino),
            Some((size, size, "0".to_string())),
            "the kernel did not store {name} inline"
        );
    }

    let mut fs = mount_rw(&image);
    let mut expected = BTreeMap::new();
    for (name, _) in files {
        let ino = ino_of(&fs, name);
        expected.insert(name, fs.read_file(ino).unwrap());
    }
    for (name, offset, len, byte) in writes {
        let ino = ino_of(&fs, name);
        assert_eq!(
            fs.write(ino, offset, &vec![byte; len])
                .unwrap_or_else(|e| panic!("writing {name} at {offset}: {e}")),
            len
        );
        let bytes = expected.get_mut(name).unwrap();
        bytes.resize(bytes.len().max(offset as usize + len), 0);
        bytes[offset as usize..offset as usize + len].fill(byte);
        assert_eq!(&fs.read_file(ino).unwrap(), bytes, "{name} reads back");
    }
    for (name, _) in files {
        let ours = fs.read_inode(ino_of(&fs, name)).unwrap();
        let theirs = fs.read_inode(ino_of(&fs, &control(name))).unwrap();
        assert_eq!(
            (ours.size, ours.nbytes),
            (theirs.size, theirs.nbytes),
            "{name}'s size and nbytes are not what the kernel's own writes gave its control"
        );
    }
    let inos: Vec<(&str, u64, u64)> = files
        .iter()
        .map(|&(name, _)| (name, ino_of(&fs, name), ino_of(&fs, &control(name))))
        .collect();
    drop(fs);
    for (name, ours, theirs) in inos {
        let (_, kernel_inline) = extent_offsets(&image, theirs);
        assert!(!kernel_inline, "the kernel kept {} inline", control(name));
        let (offsets, inline) = extent_offsets(&image, ours);
        assert!(!inline, "{name} still holds an inline item: {offsets:?}");
        assert!(
            !offsets.is_empty() && offsets.iter().all(|o| o % 4096 == 0),
            "{name}'s extents are not sector-aligned: {offsets:?}"
        );
    }

    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum after appending to inline files:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub after writes that outgrow inline data",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );
    let names: Vec<String> = files
        .iter()
        .flat_map(|&(name, _)| [name.to_string(), control(name)])
        .collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let seen = kernel_view(&image, "after appending to inline files", &refs);
    for (name, _) in files {
        assert_eq!(
            seen[name].0,
            sha256_hex(&expected[name]),
            "{name}: wrong bytes"
        );
        assert_eq!(
            seen[name].0,
            seen[&control(name)].0,
            "{name} does not read as the kernel's own writes left its control"
        );
    }
}

/// The kind (`regular`, `prealloc`, `inline`) and disk address of the
/// `EXTENT_DATA` item at `offset` in inode `ino`, as `btrfs inspect-internal
/// dump-tree` prints it, and the reference count the extent tree records
/// for that address.
fn extent_kind_and_refs(image: &Path, ino: u64, offset: u64) -> (String, u64, u64) {
    let tree = dump_tree(image, "5");
    let lines: Vec<&str> = tree.lines().collect();
    let needle = format!("key ({ino} EXTENT_DATA {offset})");
    let at = lines
        .iter()
        .position(|l| l.contains(&needle))
        .unwrap_or_else(|| panic!("inode {ino} has no extent item at {offset}\n{tree}"));
    let body: Vec<&str> = lines[at + 1..]
        .iter()
        .take_while(|l| !l.contains(" key ("))
        .copied()
        .collect();
    let kind = body
        .iter()
        .find_map(|l| {
            let open = l.find(" type ")?;
            let rest = &l[open..];
            Some(rest[rest.find('(')? + 1..rest.find(')')?].to_string())
        })
        .unwrap_or_else(|| panic!("no type for inode {ino} at {offset}: {body:?}"));
    let bytenr: u64 = body
        .iter()
        .find_map(|l| {
            let words: Vec<&str> = l.split_whitespace().collect();
            let i = words.iter().position(|w| *w == "byte")?;
            words.get(i + 1)?.parse().ok()
        })
        .unwrap_or_else(|| panic!("no disk byte for inode {ino} at {offset}: {body:?}"));
    let extents = dump_tree(image, "2");
    let lines: Vec<&str> = extents.lines().collect();
    let needle = format!("key ({bytenr} EXTENT_ITEM ");
    let at = lines
        .iter()
        .position(|l| l.contains(&needle))
        .unwrap_or_else(|| panic!("no extent record at {bytenr}\n{extents}"));
    let words: Vec<&str> = lines[at + 1].split_whitespace().collect();
    let refs = words
        .iter()
        .position(|w| *w == "refs")
        .and_then(|i| words.get(i + 1)?.parse().ok())
        .unwrap_or_else(|| panic!("no refs in {:?}", lines[at + 1]));
    (kind, bytenr, refs)
}

/// A write covering every sector of a preallocated item, in a checksummed
/// copy-on-write file whose preallocated extent only that item references,
/// is written where the extent lies and the item becomes regular, in place
/// (#535): no item is added, so the extent record's reference count does
/// not move, and the written sectors gain digests. The kernel made the same
/// write into a control `fallocate`d alongside, so the item kind and
/// reference count the driver is held to are its own; `btrfs check
/// --check-data-csum` and a kernel scrub verify the new digests, and
/// `filefrag` must report no unwritten range left in either file.
#[test]
fn a_write_covering_a_whole_preallocated_item_makes_it_regular_in_place() {
    const OURS: &str = "pre.bin";
    const THEIRS: &str = "pre-control.bin";
    const LEN: usize = 65536;
    let script = format!(
        "fallocate -l {LEN} \"$MNT/{OURS}\"\n\
         fallocate -l {LEN} \"$MNT/{THEIRS}\"\n\
         sync\n\
         head -c {LEN} /dev/zero | tr '\\000' '\\132' \
         | dd of=\"$MNT/{THEIRS}\" bs={LEN} conv=notrunc status=none\n\
         sync\n"
    );
    let scratch = Scratch::new("cow-write-prealloc-whole", &script);
    let image = scratch.image();

    let mut fs = mount_rw(&image);
    let (ino, theirs) = (ino_of(&fs, OURS), ino_of(&fs, THEIRS));
    drop(fs);
    let (kind, before, _) = extent_kind_and_refs(&image, ino, 0);
    assert_eq!(kind, "prealloc", "the kernel did not preallocate {OURS}");

    fs = mount_rw(&image);
    let data = vec![0x5au8; LEN];
    assert_eq!(
        fs.write(ino, 0, &data)
            .unwrap_or_else(|e| panic!("writing the whole preallocated range: {e}")),
        LEN
    );
    assert_eq!(fs.read_file(ino).unwrap(), data, "{OURS} reads back");
    let (ours_inode, theirs_inode) = (fs.read_inode(ino).unwrap(), fs.read_inode(theirs).unwrap());
    assert_eq!(
        (ours_inode.size, ours_inode.nbytes),
        (theirs_inode.size, theirs_inode.nbytes),
        "{OURS}'s size and nbytes are not what the kernel's write gave its control"
    );
    drop(fs);

    let (kind, after, refs) = extent_kind_and_refs(&image, ino, 0);
    let (their_kind, _, their_refs) = extent_kind_and_refs(&image, theirs, 0);
    assert_eq!(
        their_kind, "regular",
        "the kernel left its control preallocated"
    );
    assert_eq!(
        (kind.as_str(), refs),
        (their_kind.as_str(), their_refs),
        "{OURS}'s item kind and reference count differ from the control's"
    );
    assert_eq!(
        after, before,
        "the write did not land where the preallocated extent lies"
    );

    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum after writing a preallocated range:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub after writing a preallocated range",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );
    let unwritten = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "unwritten ranges after writing a preallocated range",
        &format!(
            "for f in {OURS} {THEIRS}; do \
             printf '%s %s\\n' \"$f\" \"$(filefrag -v \"$MNT/$f\" | grep -c unwritten)\"; done\n"
        ),
    );
    for line in unwritten.lines() {
        assert!(line.ends_with(" 0"), "an unwritten range is left: {line}");
    }
    let seen = kernel_view(
        &image,
        "after writing a preallocated range",
        &[OURS, THEIRS],
    );
    assert_eq!(seen[OURS].0, sha256_hex(&data), "{OURS}: wrong bytes");
    assert_eq!(
        seen[OURS].0, seen[THEIRS].0,
        "{OURS} does not read as its control"
    );
}

/// How many reflinks of `orig.bin` the keyed-reference fill makes: more
/// than a 16 KiB node's extent record holds inline (about 33 data
/// references), so the kernel files the rest as keyed `EXTENT_DATA_REF`
/// items of their own.
const KEYED_REFLINKS: usize = 48;

/// The fill for the keyed-reference test: one checksummed 64 KiB extent,
/// reflinked [`KEYED_REFLINKS`] times.
fn keyed_fill(count: usize) -> String {
    let mut script = String::from("head -c 65536 /dev/urandom > \"$MNT/orig.bin\"\nsync\n");
    for i in 0..count {
        script.push_str(&format!(
            "cp --reflink=always \"$MNT/orig.bin\" \"$MNT/r{i:02}.bin\"\n"
        ));
    }
    script.push_str("sync");
    script
}

/// The extent record of `bytenr`, from the kernel's tools: its total, and
/// each keyed `EXTENT_DATA_REF` item's inode and count.
fn keyed_refs(image: &Path, bytenr: u64) -> (u64, BTreeMap<u64, u64>) {
    let extents = dump_tree(image, "2");
    let lines: Vec<&str> = extents.lines().collect();
    let record = format!("key ({bytenr} EXTENT_ITEM ");
    let at = lines
        .iter()
        .position(|l| l.contains(&record))
        .unwrap_or_else(|| panic!("no extent record at {bytenr}\n{extents}"));
    let words: Vec<&str> = lines[at + 1].split_whitespace().collect();
    let total = words
        .iter()
        .position(|w| *w == "refs")
        .and_then(|i| words.get(i + 1)?.parse().ok())
        .unwrap_or_else(|| panic!("no refs in {:?}", lines[at + 1]));
    let keyed_key = format!("key ({bytenr} EXTENT_DATA_REF ");
    let mut keyed = BTreeMap::new();
    for (i, line) in lines.iter().enumerate() {
        if !line.contains(&keyed_key) {
            continue;
        }
        let words: Vec<&str> = lines[i + 1].split_whitespace().collect();
        let field = |name: &str| -> u64 {
            words
                .iter()
                .position(|w| *w == name)
                .and_then(|j| words.get(j + 1)?.parse().ok())
                .unwrap_or_else(|| panic!("no {name} in {:?}", lines[i + 1]))
        };
        keyed.insert(field("objectid"), field("count"));
    }
    (total, keyed)
}

/// A write into a reflink whose reference the kernel filed as a keyed
/// `EXTENT_DATA_REF` item, not inline in the extent record (#442), copies
/// the extent and drops that item -- its count down, or the item gone at a
/// count of one -- and one from the record's total. The kernel made every
/// reference; `dump-tree` shows which are keyed before the write and that
/// the written file's is gone after it, `btrfs check --check-data-csum`
/// counts every reference against the items naming it, a kernel scrub
/// verifies every digest, and the kernel reads every file back.
#[test]
fn a_write_into_a_reflink_whose_reference_is_a_keyed_item_drops_that_item() {
    write_keyed_reference(KEYED_REFLINKS, false);
}

#[test]
fn a_write_drops_a_keyed_reference_in_another_leaf_from_its_extent_record() {
    write_keyed_reference(512, true);
}

fn write_keyed_reference(count: usize, another_leaf: bool) {
    let scratch = Scratch::new("cow-write-keyed-ref", &keyed_fill(count));
    let image = scratch.image();
    let names: Vec<String> = std::iter::once("orig.bin".to_string())
        .chain((0..count).map(|i| format!("r{i:02}.bin")))
        .collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();

    let mut fs = mount_rw(&image);
    let inos: BTreeMap<&str, u64> = refs.iter().map(|n| (*n, ino_of(&fs, n))).collect();
    let bytenr = {
        let tree = dump_tree(&image, "5");
        let lines: Vec<&str> = tree.lines().collect();
        let needle = format!("key ({} EXTENT_DATA 0)", inos["orig.bin"]);
        let at = lines
            .iter()
            .position(|l| l.contains(&needle))
            .unwrap_or_else(|| panic!("orig.bin has no extent item at 0\n{tree}"));
        lines[at + 1..]
            .iter()
            .take_while(|l| !l.contains(" key ("))
            .find_map(|l| {
                let words: Vec<&str> = l.split_whitespace().collect();
                let i = words.iter().position(|w| *w == "byte")?;
                words.get(i + 1)?.parse::<u64>().ok()
            })
            .unwrap_or_else(|| panic!("no disk byte for orig.bin"))
    };

    // The premise, from the kernel's tools: some references are keyed.
    let (total, keyed) = keyed_refs(&image, bytenr);
    assert_eq!(
        total,
        count as u64 + 1,
        "the extent should count orig.bin and every reflink"
    );
    // dump-tree supplies the actual leaf address for the record and
    // each keyed item. The larger fill must exercise a different leaf.
    let dump = dump_tree(&image, "2");
    let mut leaf = 0;
    let mut record_leaf = None;
    let mut item_leaf = BTreeMap::new();
    let mut keyed_leaf = None;
    for line in dump.lines() {
        if let Some(rest) = line.strip_prefix("leaf ") {
            leaf = rest
                .split_whitespace()
                .next()
                .unwrap()
                .parse::<u64>()
                .unwrap();
        }
        if line.contains(&format!("key ({bytenr} EXTENT_ITEM ")) {
            record_leaf = Some(leaf);
        }
        if line.contains(" key (") {
            keyed_leaf = line
                .contains(&format!("key ({bytenr} EXTENT_DATA_REF "))
                .then_some(leaf);
        } else if let Some(at) = keyed_leaf {
            let words: Vec<_> = line.split_whitespace().collect();
            if let Some(i) = words.iter().position(|w| *w == "objectid") {
                item_leaf.insert(words[i + 1].parse::<u64>().unwrap(), at);
            }
        }
    }
    let record_leaf = record_leaf.expect("the extent record has a leaf");
    let (victim, &victim_ino) = inos
        .iter()
        .find(|(_, ino)| {
            keyed.get(ino) == Some(&1)
                && (!another_leaf || item_leaf.get(ino).is_some_and(|at| *at != record_leaf))
        })
        .unwrap_or_else(|| panic!("no reference is a keyed item counting one: {keyed:?}"));

    let mut expected: BTreeMap<&str, Vec<u8>> = BTreeMap::new();
    for name in &refs {
        expected.insert(name, fs.read_file(inos[name]).unwrap());
    }
    let bytes = b"a keyed reference, copied on write ".repeat(100);
    let n = fs
        .write(victim_ino, 5000, &bytes)
        .unwrap_or_else(|e| panic!("writing into {victim}, whose reference is keyed: {e}"));
    assert_eq!(n, bytes.len(), "a short write");
    expected.get_mut(victim).unwrap()[5000..5000 + bytes.len()].copy_from_slice(&bytes);
    drop(fs);

    assert_btrfs_check_clean(&image, "after a write dropping a keyed reference");
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum finds data that disagrees with its digests:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );

    let (after_total, after_keyed) = keyed_refs(&image, bytenr);
    assert_eq!(
        after_total,
        total - 1,
        "the record's total should be one lower"
    );
    assert!(
        !after_keyed.contains_key(&victim_ino),
        "{victim}'s keyed reference should be gone: {after_keyed:?}"
    );
    let mut rest = keyed.clone();
    rest.remove(&victim_ino);
    assert_eq!(
        after_keyed, rest,
        "every other keyed reference should be untouched"
    );

    let after = kernel_view(&image, "after the keyed-reference write", &refs);
    for name in &refs {
        assert_eq!(
            after[*name].0,
            sha256_hex(&expected[name]),
            "the kernel reads back different bytes from {name} than were written"
        );
    }
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub after dropping a keyed reference",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );
}

/// Two clones into the keyed-reference fill whose references hash alike:
/// each `(file, offset)` is a reflink `r{i}.bin` cloned into again at a
/// 4 KiB step past its first 64 KiB. The fill numbers `orig.bin` 257 and
/// `r{i}.bin` 258 + i, which the test reads back before trusting it.
fn colliding_clones() -> [(usize, u64); 2] {
    let mut seen: std::collections::HashMap<u64, (usize, u64)> = Default::default();
    for step in 0..4096u64 {
        for i in 0..KEYED_REFLINKS {
            let offset = 65536 + step * 4096;
            let hash = fs_btrfs::extent_write::data_ref_hash(5, 258 + i as u64, offset);
            match seen.get(&hash) {
                Some(&(j, at)) if j != i => return [(j, at), (i, offset)],
                Some(_) => {}
                None => {
                    seen.insert(hash, (i, offset));
                }
            }
        }
    }
    panic!("no two clones within the search hash alike")
}

/// Every keyed `EXTENT_DATA_REF` of `bytenr`, from the kernel's tools:
/// `(inode, reference offset)` to its key offset and count.
fn keyed_refs_by_offset(image: &Path, bytenr: u64) -> BTreeMap<(u64, u64), (u64, u64)> {
    let extents = dump_tree(image, "2");
    let lines: Vec<&str> = extents.lines().collect();
    let keyed_key = format!("key ({bytenr} EXTENT_DATA_REF ");
    let mut keyed = BTreeMap::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(at) = line.find(&keyed_key) else {
            continue;
        };
        let hash: u64 = line[at + keyed_key.len()..]
            .split(')')
            .next()
            .and_then(|w| w.trim().parse().ok())
            .unwrap_or_else(|| panic!("no key offset in {line:?}"));
        let words: Vec<&str> = lines[i + 1].split_whitespace().collect();
        let field = |name: &str| -> u64 {
            words
                .iter()
                .position(|w| *w == name)
                .and_then(|j| words.get(j + 1)?.parse().ok())
                .unwrap_or_else(|| panic!("no {name} in {:?}", lines[i + 1]))
        };
        keyed.insert((field("objectid"), field("offset")), (hash, field("count")));
    }
    keyed
}

/// The kernel files a keyed reference whose hash key is taken by another
/// reference at the next free key offset. A write into the shifted
/// reference's range finds it by its root, inode and offset, drops it at
/// its actual key, and leaves the reference at the hash alone (#608).
/// `dump-tree` shows the shift before the write and the survivors after
/// it; `btrfs check --check-data-csum`, a kernel scrub and the kernel's
/// read-back of every file hold the result to account.
#[test]
fn a_write_drops_a_keyed_reference_filed_above_its_taken_hash_key() {
    let clones = colliding_clones();
    let mut fill = keyed_fill(KEYED_REFLINKS);
    fill.push_str(
        "\ncd \"$MNT\"\npython3 - <<'PY'\nimport fcntl, os, struct\nFICLONERANGE = 0x4020940d\n",
    );
    for (i, offset) in clones {
        fill.push_str(&format!(
            "s = os.open('orig.bin', os.O_RDONLY)\nd = os.open('r{i:02}.bin', os.O_WRONLY)\n\
             fcntl.ioctl(d, FICLONERANGE, struct.pack('=qQQQ', s, 0, 65536, {offset}))\n\
             os.close(s)\nos.close(d)\nos.sync()\n"
        ));
    }
    fill.push_str("PY\nsync");
    let scratch = Scratch::new("cow-write-shifted-ref", &fill);
    let image = scratch.image();
    let names: Vec<String> = std::iter::once("orig.bin".to_string())
        .chain((0..KEYED_REFLINKS).map(|i| format!("r{i:02}.bin")))
        .collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();

    let mut fs = mount_rw(&image);
    let inos: BTreeMap<&str, u64> = refs.iter().map(|n| (*n, ino_of(&fs, n))).collect();
    for (i, _) in clones {
        assert_eq!(
            inos[refs[i + 1]],
            258 + i as u64,
            "the collision was searched for under the wrong inode numbers"
        );
    }
    let bytenr = {
        let tree = dump_tree(&image, "5");
        let lines: Vec<&str> = tree.lines().collect();
        let needle = format!("key ({} EXTENT_DATA 0)", inos["orig.bin"]);
        let at = lines
            .iter()
            .position(|l| l.contains(&needle))
            .unwrap_or_else(|| panic!("orig.bin has no extent item at 0\n{tree}"));
        lines[at + 1..]
            .iter()
            .take_while(|l| !l.contains(" key ("))
            .find_map(|l| {
                let words: Vec<&str> = l.split_whitespace().collect();
                let i = words.iter().position(|w| *w == "byte")?;
                words.get(i + 1)?.parse::<u64>().ok()
            })
            .unwrap_or_else(|| panic!("no disk byte for orig.bin"))
    };

    // The premise, from the kernel's tools: both clones are keyed, one at
    // the hash and the other shifted above it.
    let (total, _) = keyed_refs(&image, bytenr);
    let keyed = keyed_refs_by_offset(&image, bytenr);
    let hash = fs_btrfs::extent_write::data_ref_hash(5, 258 + clones[0].0 as u64, clones[0].1);
    let placed: Vec<((u64, u64), (u64, u64))> = clones
        .iter()
        .map(|(i, offset)| {
            let at = (258 + *i as u64, *offset);
            (
                at,
                *keyed
                    .get(&at)
                    .unwrap_or_else(|| panic!("the clone {at:?} is not keyed: {keyed:?}")),
            )
        })
        .collect();
    let (victim_at, _) = placed
        .iter()
        .find(|(_, (key, count))| *key == hash + 1 && *count == 1)
        .copied()
        .unwrap_or_else(|| panic!("no clone is shifted to {}: {placed:?}", hash + 1));
    assert!(
        placed.iter().any(|(_, (key, _))| *key == hash),
        "the other clone should hold the hash key: {placed:?}"
    );
    let victim = refs[(victim_at.0 - 257) as usize];

    let mut expected: BTreeMap<&str, Vec<u8>> = BTreeMap::new();
    for name in &refs {
        expected.insert(name, fs.read_file(inos[name]).unwrap());
    }
    let bytes = b"a shifted keyed reference, copied on write ".repeat(100);
    let at = victim_at.1 + 5000;
    let n = fs
        .write(victim_at.0, at, &bytes)
        .unwrap_or_else(|e| panic!("writing into {victim}, whose reference is shifted: {e}"));
    assert_eq!(n, bytes.len(), "a short write");
    expected.get_mut(victim).unwrap()[at as usize..at as usize + bytes.len()]
        .copy_from_slice(&bytes);
    drop(fs);

    assert_btrfs_check_clean(&image, "after a write dropping a shifted keyed reference");
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum finds data that disagrees with its digests:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );

    let (after_total, _) = keyed_refs(&image, bytenr);
    assert_eq!(
        after_total,
        total - 1,
        "the record's total should be one lower"
    );
    let mut rest = keyed.clone();
    rest.remove(&victim_at);
    assert_eq!(
        keyed_refs_by_offset(&image, bytenr),
        rest,
        "only the shifted reference should be gone"
    );

    let after = kernel_view(&image, "after the shifted-reference write", &refs);
    for name in &refs {
        assert_eq!(
            after[*name].0,
            sha256_hex(&expected[name]),
            "the kernel reads back different bytes from {name} than were written"
        );
    }
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub after dropping a shifted keyed reference",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );
}

/// One block of the checksum tree, from `dump-tree -t csum`.
#[derive(Debug, Default)]
struct CsumBlock {
    /// The first key's offset: for a leaf its first item's, for a node
    /// its first key pointer's.
    first: Option<u64>,
    /// A node's key pointers: (key offset, child address).
    ptrs: Vec<(u64, u64)>,
    /// A leaf's item count.
    items: usize,
    /// A leaf's digest ranges, [start, end) in logical bytes.
    ranges: Vec<(u64, u64)>,
    leaf: bool,
}

/// The checksum tree's blocks, by address.
fn csum_tree(image: &Path) -> BTreeMap<u64, CsumBlock> {
    let tree = dump_tree(image, "csum");
    let mut out: BTreeMap<u64, CsumBlock> = BTreeMap::new();
    let mut at = 0u64;
    let offset = |w: &str| -> u64 {
        w.trim_end_matches(')')
            .parse()
            .unwrap_or_else(|e| panic!("dump-tree printed a key offset {w:?}: {e}"))
    };
    for line in tree.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            [kind @ ("leaf" | "node"), addr, rest @ ..] => {
                at = addr.parse().unwrap();
                let items = rest
                    .iter()
                    .position(|w| *w == "items")
                    .and_then(|i| rest.get(i + 1)?.parse().ok())
                    .unwrap_or_else(|| panic!("no item count in {line:?}"));
                out.insert(
                    at,
                    CsumBlock {
                        items,
                        leaf: *kind == "leaf",
                        ..CsumBlock::default()
                    },
                );
            }
            ["key", _, _, off, "block", child, ..] => {
                let block = out.get_mut(&at).unwrap();
                let key = offset(off);
                block.first.get_or_insert(key);
                block.ptrs.push((key, child.parse().unwrap()));
            }
            ["item", _, "key", _, _, off, ..] => {
                out.get_mut(&at).unwrap().first.get_or_insert(offset(off));
            }
            ["range", "start", start, "end", end, ..] => {
                out.get_mut(&at)
                    .unwrap()
                    .ranges
                    .push((start.parse().unwrap(), end.parse().unwrap()));
            }
            _ => {}
        }
    }
    out
}

/// The logical byte ranges `filefrag` maps for a file, [start, end).
fn mapped_ranges(extents: &[Mapped], len: u64) -> Vec<(u64, u64)> {
    const BLOCK: u64 = 4096;
    let mut out = Vec::new();
    for (i, e) in extents.iter().enumerate() {
        let end = extents
            .get(i + 1)
            .map_or(len.div_ceil(BLOCK), |next| next.logical);
        out.push((e.physical * BLOCK, (e.physical + end - e.logical) * BLOCK));
    }
    out
}

/// Bytes the first file holds: its digests outgrow one 4 KiB checksum
/// leaf (about 990 crc32c digests) more than once over.
const FIRST_LEAF_LEN: u64 = 6 << 20;

/// Emptying every leaf below the checksum root collapses the tree to
/// the empty level-zero root that mkfs and the kernel use (#548).
#[test]
fn truncating_the_only_checksummed_file_collapses_its_checksum_root() {
    let scratch = Scratch::sized(
        "truncate-empty-csum-root",
        "crc32c",
        "no-holes",
        "4096",
        &format!("head -c {FIRST_LEAF_LEN} /dev/urandom > \"$MNT/a.bin\"\nsync"),
    );
    let image = scratch.image();
    let tree = csum_tree(&image);
    assert!(
        tree.values().filter(|b| b.leaf).count() >= 2 && tree.values().any(|b| !b.leaf),
        "the checksum tree must start with a root node over at least two leaves: {tree:?}"
    );

    let mut fs = mount_rw(&image);
    let ino = ino_of(&fs, "a.bin");
    fs.truncate(ino, 0)
        .expect("truncate all checksummed data in one transaction");
    assert!(fs.read_file(ino).unwrap().is_empty());
    drop(fs);

    assert_btrfs_check_clean(&image, "after collapsing the empty checksum root");
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "checksum verification: {}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    let tree = csum_tree(&image);
    assert_eq!(
        tree.len(),
        1,
        "the checksum tree must contain only its root: {tree:?}"
    );
    let root = tree.values().next().unwrap();
    assert!(
        root.leaf && root.items == 0 && root.ptrs.is_empty() && root.ranges.is_empty(),
        "the checksum root must be an empty level-zero leaf: {tree:?}"
    );
    let after = kernel_view(
        &image,
        "read after collapsing the checksum root",
        &["a.bin"],
    );
    assert_eq!(after["a.bin"].0, sha256_hex(&[]));
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub after collapsing the checksum root",
        "test ! -s \"$MNT/a.bin\"\nbtrfs scrub start -B \"$MNT\" >/dev/null",
    );
}

/// A write that releases every digest in the checksum tree's first leaf
/// removes that leaf, its parent's first child, and the key pointer
/// naming the parent's next child becomes the parent's first (#547).
///
/// The kernel writes two checksummed files on 4 KiB nodes, the first
/// large enough that its digests alone fill the checksum tree's first
/// leaf, and this driver overwrites the first file whole in one write.
/// Then:
///
/// - `btrfs check --readonly --check-data-csum` calls the volume clean
///   and a kernel scrub passes;
/// - the kernel reads back exactly the bytes written, in both files;
/// - `dump-tree -t csum` shows no empty leaf, no digest for the released
///   extents, and each node's key pointer equal to its child's first key.
#[test]
fn a_write_emptying_the_checksum_trees_first_leaf_removes_it_and_the_checker_agrees() {
    let fill = format!(
        "head -c {FIRST_LEAF_LEN} /dev/urandom > \"$MNT/a.bin\"\n\
         sync\n\
         head -c 262144 /dev/urandom > \"$MNT/b.bin\"\n\
         sync"
    );
    let scratch = Scratch::sized(
        "cow-write-first-csum-leaf",
        "crc32c",
        "no-holes",
        "4096",
        &fill,
    );
    let image = scratch.image();

    let before = kernel_map(&image, "before the write", &["a.bin", "b.bin"]);
    let released = mapped_ranges(&before["a.bin"].1, FIRST_LEAF_LEN);
    let tree = csum_tree(&image);
    let leaves: Vec<&CsumBlock> = tree.values().filter(|b| b.leaf).collect();
    let first = leaves
        .iter()
        .min_by_key(|b| b.first)
        .expect("the checksum tree has no leaf");
    // Without a first leaf of the first file's digests alone, in a tree
    // of more than one leaf, there is nothing for the write to empty.
    assert!(
        leaves.len() > 1
            && first.items > 0
            && first
                .ranges
                .iter()
                .all(|&(s, e)| released.iter().any(|&(rs, re)| rs <= s && e <= re)),
        "the checksum tree's first leaf does not hold only a.bin's digests, so this tests \
         nothing:\n{tree:?}\na.bin: {released:?}"
    );

    let mut fs = mount_rw(&image);
    let a = ino_of(&fs, "a.bin");
    let b = ino_of(&fs, "b.bin");
    let expected_b = fs.read_file(b).unwrap();
    let bytes: Vec<u8> = (0..FIRST_LEAF_LEN)
        .map(|i| (i * 7 + i / 4093) as u8)
        .collect();
    let n = fs
        .write(a, 0, &bytes)
        .unwrap_or_else(|e| panic!("overwriting a.bin whole: {e}"));
    assert_eq!(n, bytes.len(), "a short write");
    assert_eq!(
        fs.read_file(a).unwrap(),
        bytes,
        "a.bin reads back differently"
    );
    assert_eq!(
        fs.read_file(b).unwrap(),
        expected_b,
        "b.bin reads back differently"
    );
    drop(fs);

    assert_btrfs_check_clean(&image, "after emptying the first checksum leaf");
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum finds data that disagrees with its digests:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );

    let tree = csum_tree(&image);
    for (at, block) in &tree {
        assert!(
            block.items > 0,
            "the checksum tree keeps an empty block at {at}:\n{tree:?}"
        );
        for &(s, e) in &block.ranges {
            assert!(
                released.iter().all(|&(rs, re)| e <= rs || re <= s),
                "the checksum tree keeps a digest of a released extent, [{s}, {e}):\n{tree:?}"
            );
        }
        for &(key, child) in &block.ptrs {
            let below = tree
                .get(&child)
                .unwrap_or_else(|| panic!("the node at {at} points at {child}, not in the tree"));
            assert_eq!(
                Some(key),
                below.first,
                "the node at {at} names {child} by a key other than its first:\n{tree:?}"
            );
        }
    }

    let after = kernel_view(&image, "after the write", &["a.bin", "b.bin"]);
    assert_eq!(
        after["a.bin"].0,
        sha256_hex(&bytes),
        "the kernel reads back different bytes from a.bin than were written"
    );
    assert_eq!(
        after["b.bin"].0,
        sha256_hex(&expected_b),
        "the kernel reads back different bytes from b.bin than it wrote"
    );
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub after emptying the first checksum leaf",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );
}

/// Bytes the multi-way split's write puts in one file: 3072 crc32c
/// digests on 4 KiB sectors, more than three 4 KiB checksum leaves hold
/// (about 990 each), so the one leaf they land in divides four ways.
const MULTI_LEAF_LEN: u64 = 12 << 20;

/// One whole-file overwrite removes leaves holding only the old digests
/// and splits the receiving checksum leaf more than once (#616, #550).
/// The kernel makes the original 12 MiB extent and a small control file;
/// its replacement needs 3072 digests, or at least four 4 KiB leaves.
/// This stays far below a full root node, leaving multi-level growth to
/// #617. All four oracles must agree on the same committed image.
#[test]
fn one_overwrite_removes_old_checksum_leaves_and_splits_the_receiving_leaf_as_often_as_needed() {
    let fill = format!(
        "head -c {MULTI_LEAF_LEN} /dev/urandom > \"$MNT/a.bin\"\n\
         sync\n\
         head -c 262144 /dev/urandom > \"$MNT/b.bin\"\n\
         sync"
    );
    let scratch = Scratch::sized(
        "cow-overwrite-remove-multi-split",
        "crc32c",
        "no-holes",
        "4096",
        &fill,
    );
    let image = scratch.image();
    let before = kernel_map(&image, "before the combined overwrite", &["a.bin", "b.bin"]);
    assert_eq!(
        before["a.bin"].1.len(),
        1,
        "the kernel must make one extent so its replacement inserts all 3072 digests together"
    );
    assert!(
        !before["a.bin"].1[0].shared,
        "the old extent must be unshared so the overwrite releases its digests"
    );
    let released = mapped_ranges(&before["a.bin"].1, MULTI_LEAF_LEN);
    let original = csum_tree(&image);
    let exclusive_leaves = original
        .values()
        .filter(|block| {
            block.leaf
                && !block.ranges.is_empty()
                && block
                    .ranges
                    .iter()
                    .all(|&(s, e)| released.iter().any(|&(rs, re)| rs <= s && e <= re))
        })
        .count();
    assert!(
        exclusive_leaves >= 2,
        "at least two old leaves must hold only a.bin's digests to exercise removal:\n\
         {original:?}\na.bin: {released:?}"
    );
    let receiving_first = original
        .values()
        .filter(|block| block.leaf)
        .filter_map(|block| block.first)
        .max()
        .expect("the original checksum tree has no nonempty leaf");

    let mut fs = mount_rw(&image);
    let a = ino_of(&fs, "a.bin");
    let inode = fs.read_inode(a).unwrap();
    assert_eq!(inode.size, MULTI_LEAF_LEN, "this must be an overwrite");
    assert_eq!(
        inode.flags & (fs_btrfs::write::INODE_NODATACOW | fs_btrfs::write::INODE_NODATASUM),
        0,
        "the file must be copy-on-write and checksummed"
    );
    let bytes: Vec<u8> = (0..MULTI_LEAF_LEN)
        .map(|i| (i * 17 + i / 4093) as u8)
        .collect();
    let generation = fs.superblock().generation;
    let n = fs
        .write(a, 0, &bytes)
        .unwrap_or_else(|e| panic!("overwriting all 12 MiB in one transaction: {e}"));
    assert_eq!(n, bytes.len(), "a short write");
    assert_eq!(
        fs.superblock().generation,
        generation + 1,
        "leaf removal and repeated splitting must commit in the same transaction"
    );
    assert_eq!(fs.read_file(a).unwrap(), bytes);
    drop(fs);

    assert_btrfs_check_clean(&image, "after checksum leaf removal and repeated splitting");
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum after the combined overwrite:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );

    let after = kernel_map(&image, "after the combined overwrite", &["a.bin", "b.bin"]);
    assert_eq!(
        after["a.bin"].0,
        sha256_hex(&bytes),
        "the kernel must read back exactly the replacement bytes"
    );
    assert_eq!(
        after["b.bin"], before["b.bin"],
        "the control file's bytes and physical mapping must stay unchanged"
    );
    assert_eq!(
        after["a.bin"].1.len(),
        1,
        "the replacement must insert one extent's 3072 digests"
    );
    let written = mapped_ranges(&after["a.bin"].1, MULTI_LEAF_LEN);
    assert!(
        written.iter().all(|&(s, e)| {
            s >= receiving_first && released.iter().all(|&(rs, re)| e <= rs || re <= s)
        }),
        "all new digests must land in the original last leaf, outside the released extent:\n\
         new: {written:?}\nold: {released:?}\nlast leaf first key: {receiving_first}"
    );

    let tree = csum_tree(&image);
    assert!(
        tree.values().filter(|block| block.leaf).count() >= 4,
        "3072 replacement digests need at least four 4 KiB leaves:\n{tree:?}"
    );
    let mut covered = Vec::new();
    for (at, block) in &tree {
        assert!(
            block.items > 0,
            "the checksum tree keeps an empty block at {at}:\n{tree:?}"
        );
        for &(s, e) in &block.ranges {
            assert!(
                released.iter().all(|&(rs, re)| e <= rs || re <= s),
                "the checksum tree keeps old digests in [{s}, {e}):\n{tree:?}"
            );
            covered.push((s, e));
        }
        for &(key, child) in &block.ptrs {
            let below = tree
                .get(&child)
                .unwrap_or_else(|| panic!("the node at {at} points outside the tree at {child}"));
            assert_eq!(
                Some(key),
                below.first,
                "the node at {at} must name child {child} by its first key:\n{tree:?}"
            );
        }
    }
    for &(start, end) in &written {
        let mut at = start;
        while at < end {
            at = covered
                .iter()
                .find(|&&(s, e)| s <= at && at < e)
                .map(|&(_, e)| e)
                .unwrap_or_else(|| {
                    panic!("no digest covers new sector at {at} in [{start}, {end}):\n{tree:?}")
                });
        }
    }
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub after checksum leaf removal and repeated splitting",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );
}

/// A write whose new digests fill more than two checksum leaves splits
/// the leaf they land in as many times as it needs, in one transaction
/// (#615, part of #550).
///
/// The kernel writes a small checksummed file and an empty one on 4 KiB
/// nodes, so the checksum tree is one leaf with room to spare, and this
/// driver writes 12 MiB into the empty file in one write. Then:
///
/// - `btrfs check --readonly --check-data-csum` calls the volume clean
///   and a kernel scrub passes;
/// - the kernel reads back exactly the bytes written, in both files;
/// - `dump-tree -t csum` shows at least four leaves, no empty block,
///   every sector of the new extents covered by a digest, and each
///   node's key pointer equal to its child's first key.
#[test]
fn a_write_adding_more_than_two_leaves_of_digests_splits_the_checksum_leaf_as_often_as_needed() {
    let fill = "head -c 262144 /dev/urandom > \"$MNT/b.bin\"\n\
                : > \"$MNT/c.bin\"\n\
                sync";
    let scratch = Scratch::sized("cow-write-multi-split", "crc32c", "no-holes", "4096", fill);
    let image = scratch.image();

    let tree = csum_tree(&image);
    assert_eq!(
        tree.values().filter(|b| b.leaf).count(),
        1,
        "the checksum tree must start as one leaf for the write to land in: {tree:?}"
    );

    let mut fs = mount_rw(&image);
    let b = ino_of(&fs, "b.bin");
    let c = ino_of(&fs, "c.bin");
    let expected_b = fs.read_file(b).unwrap();
    let bytes: Vec<u8> = (0..MULTI_LEAF_LEN)
        .map(|i| (i * 13 + i / 4091) as u8)
        .collect();
    let n = fs
        .write(c, 0, &bytes)
        .unwrap_or_else(|e| panic!("writing 12 MiB into c.bin in one write: {e}"));
    assert_eq!(n, bytes.len(), "a short write");
    assert_eq!(
        fs.read_file(c).unwrap(),
        bytes,
        "c.bin reads back differently"
    );
    assert_eq!(
        fs.read_file(b).unwrap(),
        expected_b,
        "b.bin reads back differently"
    );
    drop(fs);

    assert_btrfs_check_clean(&image, "after splitting the checksum leaf four ways");
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum finds data that disagrees with its digests:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );

    let written = mapped_ranges(
        &kernel_map(&image, "after the write", &["c.bin"])["c.bin"].1,
        MULTI_LEAF_LEN,
    );
    let tree = csum_tree(&image);
    assert!(
        tree.values().filter(|b| b.leaf).count() >= 4,
        "3072 digests need at least four 4 KiB checksum leaves:\n{tree:?}"
    );
    let mut covered: Vec<(u64, u64)> = Vec::new();
    for (at, block) in &tree {
        assert!(
            block.items > 0,
            "the checksum tree keeps an empty block at {at}:\n{tree:?}"
        );
        covered.extend(block.ranges.iter().copied());
        for &(key, child) in &block.ptrs {
            let below = tree
                .get(&child)
                .unwrap_or_else(|| panic!("the node at {at} points at {child}, not in the tree"));
            assert_eq!(
                Some(key),
                below.first,
                "the node at {at} names {child} by a key other than its first:\n{tree:?}"
            );
        }
    }
    covered.sort_unstable();
    for &(start, end) in &written {
        let mut at = start;
        while at < end {
            let next = covered
                .iter()
                .find(|&&(s, e)| s <= at && at < e)
                .map(|&(_, e)| e)
                .unwrap_or_else(|| {
                    panic!(
                        "no digest covers byte {at} of c.bin's extent [{start}, {end}):\n{tree:?}"
                    )
                });
            at = next;
        }
    }

    let after = kernel_view(&image, "after the write", &["b.bin", "c.bin"]);
    assert_eq!(
        after["c.bin"].0,
        sha256_hex(&bytes),
        "the kernel reads back different bytes from c.bin than were written"
    );
    assert_eq!(
        after["b.bin"].0,
        sha256_hex(&expected_b),
        "the kernel reads back different bytes from b.bin than it wrote"
    );
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub after splitting the checksum leaf four ways",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );
}

/// A write covering only the leading sectors of a preallocated item in one
/// checksummed copy-on-write file, and only the trailing sectors in
/// another, splits each item in two over the same extent (#536): a regular
/// item for the written sectors and a preallocated one for the rest, the
/// extent record's inline `EXTENT_DATA_REF` counted up by one, and only the
/// written sectors gaining digests. The kernel made the same two writes
/// into controls `fallocate`d alongside, so the item kinds and reference
/// counts the driver is held to are its own; `btrfs check
/// --check-data-csum` and a kernel scrub verify the digests, `filefrag`
/// must report the same unwritten ranges as the controls, and the kernel
/// must read the written bytes and zeros for the rest.
#[test]
fn a_write_at_the_start_or_end_of_a_preallocated_item_splits_it_in_two() {
    const HEAD: &str = "pre-head.bin";
    const HEAD_CONTROL: &str = "pre-head-control.bin";
    const TAIL: &str = "pre-tail.bin";
    const TAIL_CONTROL: &str = "pre-tail-control.bin";
    const LEN: usize = 65536;
    const PART: usize = 16384;
    let script = format!(
        "for f in {HEAD} {HEAD_CONTROL} {TAIL} {TAIL_CONTROL}; do \
         fallocate -l {LEN} \"$MNT/$f\"; done\n\
         sync\n\
         head -c {PART} /dev/zero | tr '\\000' '\\132' \
         | dd of=\"$MNT/{HEAD_CONTROL}\" bs={PART} conv=notrunc status=none\n\
         head -c {PART} /dev/zero | tr '\\000' '\\132' \
         | dd of=\"$MNT/{TAIL_CONTROL}\" bs={PART} seek={seek} conv=notrunc status=none\n\
         sync\n",
        seek = (LEN - PART) / PART
    );
    let scratch = Scratch::new("cow-write-prealloc-split", &script);
    let image = scratch.image();

    let mut fs = mount_rw(&image);
    let (head, head_control) = (ino_of(&fs, HEAD), ino_of(&fs, HEAD_CONTROL));
    let (tail, tail_control) = (ino_of(&fs, TAIL), ino_of(&fs, TAIL_CONTROL));
    drop(fs);
    let (kind, head_before, _) = extent_kind_and_refs(&image, head, 0);
    assert_eq!(kind, "prealloc", "the kernel did not preallocate {HEAD}");
    let (kind, tail_before, _) = extent_kind_and_refs(&image, tail, 0);
    assert_eq!(kind, "prealloc", "the kernel did not preallocate {TAIL}");

    fs = mount_rw(&image);
    let data = vec![0x5au8; PART];
    for (file, at, name) in [(head, 0, HEAD), (tail, LEN - PART, TAIL)] {
        assert_eq!(
            fs.write(file, at as u64, &data)
                .unwrap_or_else(|e| panic!("writing part of {name}'s preallocated range: {e}")),
            PART
        );
    }
    let mut head_bytes = vec![0u8; LEN];
    head_bytes[..PART].copy_from_slice(&data);
    let mut tail_bytes = vec![0u8; LEN];
    tail_bytes[LEN - PART..].copy_from_slice(&data);
    assert_eq!(fs.read_file(head).unwrap(), head_bytes, "{HEAD} reads back");
    assert_eq!(fs.read_file(tail).unwrap(), tail_bytes, "{TAIL} reads back");
    for (ours, theirs, name) in [(head, head_control, HEAD), (tail, tail_control, TAIL)] {
        let (a, b) = (fs.read_inode(ours).unwrap(), fs.read_inode(theirs).unwrap());
        assert_eq!(
            (a.size, a.nbytes),
            (b.size, b.nbytes),
            "{name}'s size and nbytes are not what the kernel's write gave its control"
        );
    }
    drop(fs);

    let tree = dump_tree(&image, "5");
    for (ours, theirs, before, name) in [
        (head, head_control, head_before, HEAD),
        (tail, tail_control, tail_before, TAIL),
    ] {
        let starts = |ino: u64| -> Vec<u64> {
            [0, PART as u64, (LEN - PART) as u64]
                .into_iter()
                .filter(|at| tree.contains(&format!("key ({ino} EXTENT_DATA {at})")))
                .collect()
        };
        assert_eq!(
            starts(ours),
            starts(theirs),
            "{name}'s items do not start where the control's do"
        );
        assert_eq!(
            starts(theirs).len(),
            2,
            "the kernel did not split {name}'s control"
        );
        for at in starts(theirs) {
            let (kind, bytenr, refs) = extent_kind_and_refs(&image, ours, at);
            let (their_kind, _, their_refs) = extent_kind_and_refs(&image, theirs, at);
            assert_eq!(
                (kind.as_str(), refs),
                (their_kind.as_str(), their_refs),
                "{name}'s item at {at}: kind and reference count differ from the control's"
            );
            assert_eq!(
                bytenr, before,
                "{name}'s item at {at} left the preallocated extent"
            );
        }
    }
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum after splitting preallocated items:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub after splitting preallocated items",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );
    let unwritten = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "unwritten ranges after splitting preallocated items",
        &format!(
            "for f in {HEAD} {HEAD_CONTROL} {TAIL} {TAIL_CONTROL}; do \
             printf '%s %s\\n' \"$f\" \"$(filefrag -v \"$MNT/$f\" | awk '/unwritten/ {{print $2, $4}}' | tr '\\n' ' ')\"; done\n"
        ),
    );
    let ranges: BTreeMap<&str, &str> = unwritten
        .lines()
        .filter_map(|l| l.split_once(' '))
        .collect();
    for (ours, theirs) in [(HEAD, HEAD_CONTROL), (TAIL, TAIL_CONTROL)] {
        assert!(
            !ranges
                .get(theirs)
                .copied()
                .unwrap_or_default()
                .trim()
                .is_empty(),
            "the kernel left no unwritten range in {theirs}:\n{unwritten}"
        );
        assert_eq!(
            ranges.get(ours).map(|r| r.trim()),
            ranges.get(theirs).map(|r| r.trim()),
            "{ours}'s unwritten ranges differ from {theirs}'s:\n{unwritten}"
        );
    }
    let seen = kernel_view(
        &image,
        "after splitting preallocated items",
        &[HEAD, HEAD_CONTROL, TAIL, TAIL_CONTROL],
    );
    assert_eq!(seen[HEAD].0, sha256_hex(&head_bytes), "{HEAD}: wrong bytes");
    assert_eq!(seen[TAIL].0, sha256_hex(&tail_bytes), "{TAIL}: wrong bytes");
    assert_eq!(
        seen[HEAD].0, seen[HEAD_CONTROL].0,
        "{HEAD} does not read as its control"
    );
    assert_eq!(
        seen[TAIL].0, seen[TAIL_CONTROL].0,
        "{TAIL} does not read as its control"
    );
}

/// A write strictly inside a preallocated item in a checksummed
/// copy-on-write file, touching neither its first nor its last sector,
/// splits the item in three over the same extent (#537): preallocated
/// before the write, regular for the written sectors, preallocated after,
/// the extent record's inline `EXTENT_DATA_REF` counted up by two, and only
/// the written sectors gaining digests. The kernel made the same write into
/// a control `fallocate`d alongside, so the item kinds and reference counts
/// the driver is held to are its own; `btrfs check --check-data-csum` and a
/// kernel scrub verify the digests, `filefrag` must report the same
/// unwritten ranges as the control, and the kernel must read the written
/// bytes and zeros on both sides.
#[test]
fn a_write_in_the_middle_of_a_preallocated_item_splits_it_in_three() {
    const MID: &str = "pre-mid.bin";
    const MID_CONTROL: &str = "pre-mid-control.bin";
    const LEN: usize = 65536;
    const PART: usize = 16384;
    let script = format!(
        "for f in {MID} {MID_CONTROL}; do fallocate -l {LEN} \"$MNT/$f\"; done\n\
         sync\n\
         head -c {PART} /dev/zero | tr '\\000' '\\132' \
         | dd of=\"$MNT/{MID_CONTROL}\" bs={PART} seek=1 conv=notrunc status=none\n\
         sync\n"
    );
    let scratch = Scratch::new("cow-write-prealloc-split-middle", &script);
    let image = scratch.image();

    let mut fs = mount_rw(&image);
    let (mid, mid_control) = (ino_of(&fs, MID), ino_of(&fs, MID_CONTROL));
    drop(fs);
    let (kind, before, _) = extent_kind_and_refs(&image, mid, 0);
    assert_eq!(kind, "prealloc", "the kernel did not preallocate {MID}");

    fs = mount_rw(&image);
    let data = vec![0x5au8; PART];
    assert_eq!(
        fs.write(mid, PART as u64, &data)
            .unwrap_or_else(|e| panic!("writing the middle of {MID}'s preallocated range: {e}")),
        PART
    );
    let mut bytes = vec![0u8; LEN];
    bytes[PART..2 * PART].copy_from_slice(&data);
    assert_eq!(fs.read_file(mid).unwrap(), bytes, "{MID} reads back");
    let (a, b) = (
        fs.read_inode(mid).unwrap(),
        fs.read_inode(mid_control).unwrap(),
    );
    assert_eq!(
        (a.size, a.nbytes),
        (b.size, b.nbytes),
        "{MID}'s size and nbytes are not what the kernel's write gave its control"
    );
    drop(fs);

    let tree = dump_tree(&image, "5");
    let starts = |ino: u64| -> Vec<u64> {
        [0, PART as u64, 2 * PART as u64]
            .into_iter()
            .filter(|at| tree.contains(&format!("key ({ino} EXTENT_DATA {at})")))
            .collect()
    };
    assert_eq!(
        starts(mid),
        starts(mid_control),
        "{MID}'s items do not start where the control's do"
    );
    assert_eq!(
        starts(mid_control).len(),
        3,
        "the kernel did not split {MID}'s control in three"
    );
    for at in starts(mid_control) {
        let (kind, bytenr, refs) = extent_kind_and_refs(&image, mid, at);
        let (their_kind, _, their_refs) = extent_kind_and_refs(&image, mid_control, at);
        assert_eq!(
            (kind.as_str(), refs),
            (their_kind.as_str(), their_refs),
            "{MID}'s item at {at}: kind and reference count differ from the control's"
        );
        assert_eq!(
            bytenr, before,
            "{MID}'s item at {at} left the preallocated extent"
        );
    }
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum after splitting a preallocated item in three:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub after splitting a preallocated item in three",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );
    let unwritten = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "unwritten ranges after splitting a preallocated item in three",
        &format!(
            "for f in {MID} {MID_CONTROL}; do \
             printf '%s %s\\n' \"$f\" \"$(filefrag -v \"$MNT/$f\" | awk '/unwritten/ {{print $2, $4}}' | tr '\\n' ' ')\"; done\n"
        ),
    );
    let ranges: BTreeMap<&str, &str> = unwritten
        .lines()
        .filter_map(|l| l.split_once(' '))
        .collect();
    assert!(
        !ranges
            .get(MID_CONTROL)
            .copied()
            .unwrap_or_default()
            .trim()
            .is_empty(),
        "the kernel left no unwritten range in {MID_CONTROL}:\n{unwritten}"
    );
    assert_eq!(
        ranges.get(MID).map(|r| r.trim()),
        ranges.get(MID_CONTROL).map(|r| r.trim()),
        "{MID}'s unwritten ranges differ from {MID_CONTROL}'s:\n{unwritten}"
    );
    let seen = kernel_view(
        &image,
        "after splitting a preallocated item in three",
        &[MID, MID_CONTROL],
    );
    assert_eq!(seen[MID].0, sha256_hex(&bytes), "{MID}: wrong bytes");
    assert_eq!(
        seen[MID].0, seen[MID_CONTROL].0,
        "{MID} does not read as its control"
    );
}

/// One write spanning the end of a regular extent and the start of the
/// preallocated item after it, in a checksummed copy-on-write file, is
/// written as the kernel writes it (#638): the regular sectors to a new
/// extent, the preallocated ones where their extent lies, the preallocated
/// item split at the write's end. The kernel made the same write into a
/// control built alongside, so the item kinds and reference counts the
/// driver is held to are its own; `btrfs check --check-data-csum` and a
/// kernel scrub verify the digests, `filefrag` must report the same
/// unwritten ranges as the control, and the kernel must read the written
/// bytes, the untouched regular bytes and zeros past the write.
#[test]
fn a_write_spanning_a_regular_extent_and_a_preallocated_item_is_written() {
    const SPAN: &str = "pre-span.bin";
    const SPAN_CONTROL: &str = "pre-span-control.bin";
    const REG: usize = 16384;
    const PRE: usize = 65536;
    const HALF: usize = 8192;
    let script = format!(
        "for f in {SPAN} {SPAN_CONTROL}; do\n\
         head -c {REG} /dev/zero | tr '\\000' '\\101' > \"$MNT/$f\"\n\
         sync\n\
         fallocate -o {REG} -l {PRE} \"$MNT/$f\"\n\
         done\n\
         sync\n\
         head -c {REG} /dev/zero | tr '\\000' '\\132' \
         | dd of=\"$MNT/{SPAN_CONTROL}\" bs={HALF} seek=1 conv=notrunc status=none\n\
         sync\n"
    );
    let scratch = Scratch::new("cow-write-prealloc-span-regular", &script);
    let image = scratch.image();

    let mut fs = mount_rw(&image);
    let (span, span_control) = (ino_of(&fs, SPAN), ino_of(&fs, SPAN_CONTROL));
    drop(fs);
    let (kind, _, _) = extent_kind_and_refs(&image, span, 0);
    assert_eq!(
        kind, "regular",
        "the kernel did not write {SPAN}'s first extent"
    );
    let (kind, before, _) = extent_kind_and_refs(&image, span, REG as u64);
    assert_eq!(kind, "prealloc", "the kernel did not preallocate {SPAN}");

    fs = mount_rw(&image);
    let data = vec![0x5au8; REG];
    assert_eq!(
        fs.write(span, HALF as u64, &data)
            .unwrap_or_else(|e| panic!(
                "writing across {SPAN}'s regular extent into its preallocated range: {e}"
            )),
        REG
    );
    let mut bytes = vec![0u8; REG + PRE];
    bytes[..HALF].fill(0x41);
    bytes[HALF..HALF + REG].copy_from_slice(&data);
    assert_eq!(fs.read_file(span).unwrap(), bytes, "{SPAN} reads back");
    let (a, b) = (
        fs.read_inode(span).unwrap(),
        fs.read_inode(span_control).unwrap(),
    );
    assert_eq!(
        (a.size, a.nbytes),
        (b.size, b.nbytes),
        "{SPAN}'s size and nbytes are not what the kernel's write gave its control"
    );
    drop(fs);

    let tree = dump_tree(&image, "5");
    let starts = |ino: u64| -> Vec<u64> {
        [0, HALF as u64, REG as u64, (HALF + REG) as u64]
            .into_iter()
            .filter(|at| tree.contains(&format!("key ({ino} EXTENT_DATA {at})")))
            .collect()
    };
    assert_eq!(
        starts(span),
        starts(span_control),
        "{SPAN}'s items do not start where the control's do"
    );
    for at in starts(span_control) {
        let (kind, bytenr, refs) = extent_kind_and_refs(&image, span, at);
        let (their_kind, _, their_refs) = extent_kind_and_refs(&image, span_control, at);
        assert_eq!(
            (kind.as_str(), refs),
            (their_kind.as_str(), their_refs),
            "{SPAN}'s item at {at}: kind and reference count differ from the control's"
        );
        if at >= REG as u64 {
            assert_eq!(
                bytenr, before,
                "{SPAN}'s item at {at} left the preallocated extent"
            );
        }
    }
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum after a write spanning a regular and a preallocated item:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub after a write spanning a regular and a preallocated item",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );
    let unwritten = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "unwritten ranges after a write spanning a regular and a preallocated item",
        &format!(
            "for f in {SPAN} {SPAN_CONTROL}; do \
             printf '%s %s\\n' \"$f\" \"$(filefrag -v \"$MNT/$f\" | awk '/unwritten/ {{print $2, $4}}' | tr '\\n' ' ')\"; done\n"
        ),
    );
    let ranges: BTreeMap<&str, &str> = unwritten
        .lines()
        .filter_map(|l| l.split_once(' '))
        .collect();
    assert!(
        !ranges
            .get(SPAN_CONTROL)
            .copied()
            .unwrap_or_default()
            .trim()
            .is_empty(),
        "the kernel left no unwritten range in {SPAN_CONTROL}:\n{unwritten}"
    );
    assert_eq!(
        ranges.get(SPAN).map(|r| r.trim()),
        ranges.get(SPAN_CONTROL).map(|r| r.trim()),
        "{SPAN}'s unwritten ranges differ from {SPAN_CONTROL}'s:\n{unwritten}"
    );
    let seen = kernel_view(
        &image,
        "after a write spanning a regular and a preallocated item",
        &[SPAN, SPAN_CONTROL],
    );
    assert_eq!(seen[SPAN].0, sha256_hex(&bytes), "{SPAN}: wrong bytes");
    assert_eq!(
        seen[SPAN].0, seen[SPAN_CONTROL].0,
        "{SPAN} does not read as its control"
    );
}
