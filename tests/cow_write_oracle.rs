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
//! keeps those references (#261); an extent a snapshot shares is still
//! refused. A write into a hole adds an extent, and so does one that
//! starts at or before the end of a file and grows it (#432), or one that
//! starts past it, the rest of the old last sector zeroed first (#433).
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
        let dir = PathBuf::from(temp_path!("{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("fs.img");
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(256 << 20))
            .unwrap();
        let made = oracle("mkfs.btrfs")
            .args([
                "-q", "-f", "-s", "4096", "-n", "16384", "--csum", algorithm, "-O", features,
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

/// An extent a snapshot still reads is refused, and the image is left
/// byte for byte as it was: releasing it would free what the snapshot
/// reads.
#[test]
fn a_copy_on_write_extent_a_snapshot_shares_is_refused_and_untouched() {
    let scratch = Scratch::new(
        "cow-write-snapshot",
        "mount -o remount,nodatasum \"$MNT\"\n\
         head -c 65536 /dev/urandom > \"$MNT/one.bin\"\n\
         sync\n\
         btrfs subvolume snapshot -r \"$MNT\" \"$MNT/snap\" >/dev/null\n\
         sync",
    );
    let image = scratch.image();
    let before = std::fs::read(&image).unwrap();

    let mut fs = mount_rw(&image);
    let ino = ino_of(&fs, ONE);
    let err = fs
        .write(ino, 4096, b"this must not land")
        .expect_err("an extent a snapshot shares must be refused");
    assert!(
        err.to_string().contains("snapshot"),
        "the refusal should name the snapshot: {err}"
    );
    drop(fs);
    assert!(
        std::fs::read(&image).unwrap() == before,
        "a refused write changed the image"
    );
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
    let tree = dump_tree(image, "5");
    let lines: Vec<&str> = tree.lines().collect();
    let needle = format!("key ({ino} EXTENT_DATA ");
    let mut end = 0;
    let mut holes = Vec::new();
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
        if field("extent data disk byte", "byte") == 0 {
            holes.push((offset, len));
        }
    }
    assert_eq!(
        end, 196_608,
        "inode {ino}: incomplete extent coverage\n{tree}"
    );
    holes
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
///
/// A write that starts past the end is still refused (#433), before
/// anything is written.
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

    // A write that starts past the end, leaving a gap, is
    // a_write_that_starts_past_the_end_zeroes_the_old_tail_as_the_kernel_does's
    // (#433).

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

/// The fill for writes that start past a file's end (#433): each file and
/// its control hold the same random bytes, checksummed. `gap` and `near`
/// end partway through a sector, at 40,000. `stale` is 40,960 bytes, which
/// this crate cuts to 37,000 and the kernel cuts its control to 37,000.
/// Then the kernel makes each control's write, as `dd` does past an end.
const GAP_FILL: &str =
    "head -c 40000 /dev/urandom | tee \"$MNT/gap.bin\" > \"$MNT/gap-control.bin\"\n\
     head -c 40000 /dev/urandom | tee \"$MNT/near.bin\" > \"$MNT/near-control.bin\"\n\
     head -c 40960 /dev/urandom | tee \"$MNT/stale.bin\" > \"$MNT/stale-control.bin\"\n\
     sync\n\
     truncate -s 37000 \"$MNT/stale-control.bin\"\n\
     sync\n\
     head -c 3000 /dev/zero | tr '\\000' '\\245' \
     | dd of=\"$MNT/gap-control.bin\" bs=1 seek=52388 conv=notrunc status=none\n\
     head -c 500 /dev/zero | tr '\\000' '\\132' \
     | dd of=\"$MNT/near-control.bin\" bs=1 seek=40100 conv=notrunc status=none\n\
     head -c 2000 /dev/zero | tr '\\000' '\\074' \
     | dd of=\"$MNT/stale-control.bin\" bs=1 seek=45000 conv=notrunc status=none\n\
     sync";

/// The logical address of the extent `ino`'s item at file offset 0 names,
/// from btrfs-progs' own dump of the fs tree.
fn first_extent(image: &Path, ino: u64) -> u64 {
    let tree = dump_tree(image, "5");
    let item = tree
        .split(&format!("key ({ino} EXTENT_DATA 0)"))
        .nth(1)
        .unwrap_or_else(|| panic!("inode {ino} has no extent at offset 0"));
    item.lines()
        .find(|line| line.contains("disk byte"))
        .and_then(|line| line.split("disk byte ").nth(1))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|w| w.parse().ok())
        .unwrap_or_else(|| panic!("inode {ino}'s first extent names no disk byte:\n{item}"))
}

/// A write that starts past the end of a file (#433): the rest of the old
/// last sector is zeroed, as the kernel zeroes it before it extends a file,
/// the gap is an implicit hole with no item, and the write's own sectors
/// are added. `stale` holds non-zero bytes on disk past its end, shown
/// raw before the write, so a write that skipped the zeroing reads them
/// back. Each file must end as its control does, which the kernel wrote.
#[test]
fn a_write_that_starts_past_the_end_zeroes_the_old_tail_as_the_kernel_does() {
    const GAP: &str = "gap.bin";
    const NEAR: &str = "near.bin";
    const STALE: &str = "stale.bin";
    let control = |name: &str| name.replace(".bin", "-control.bin");
    let scratch = Scratch::new("cow-write-past-end", GAP_FILL);
    let image = scratch.image();

    let mut fs = mount_rw(&image);
    let mut expected: BTreeMap<&str, Vec<u8>> = BTreeMap::new();
    for name in [GAP, NEAR, STALE] {
        let ino = ino_of(&fs, name);
        assert_eq!(
            fs.read_inode(ino).unwrap().flags & fs_btrfs::write::INODE_NODATASUM,
            0,
            "{name} must be checksummed for this to test anything"
        );
        expected.insert(name, fs.read_file(ino).unwrap());
    }

    // `stale`, cut to 37,000 by this crate: its extent stays whole, and the
    // bytes past the new end are still the old ones on disk.
    let stale = ino_of(&fs, STALE);
    fs.truncate(stale, 37_000)
        .expect("truncate stale.bin to 37000");
    expected.get_mut(STALE).unwrap().truncate(37_000);
    drop(fs);
    let old_last_sector = first_extent(&image, stale) + 36_864;
    let raw = {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(&image).unwrap())).unwrap();
        let physical = fs
            .chunk_map()
            .map(old_last_sector)
            .expect("the old last sector's address")
            .physical;
        let mut sector = vec![0u8; 4096];
        use std::os::unix::fs::FileExt;
        std::fs::File::open(&image)
            .unwrap()
            .read_exact_at(&mut sector, physical)
            .unwrap();
        sector
    };
    assert!(
        raw[37_000 - 36_864..].iter().any(|&b| b != 0),
        "stale.bin's bytes past its end are zeros on disk, so this case tests nothing"
    );

    let mut fs = mount_rw(&image);
    let writes: Vec<(&str, u64, Vec<u8>)> = vec![
        (GAP, 52_388, vec![0xa5; 3000]),
        (NEAR, 40_100, vec![0x5a; 500]),
        (STALE, 45_000, vec![0x3c; 2000]),
    ];
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
        file.resize(*offset as usize, 0);
        file.extend_from_slice(bytes);
        assert_eq!(
            &fs.read_file(ino).unwrap(),
            file,
            "{name} reads back differently through this driver after the write"
        );
    }
    for name in [GAP, NEAR, STALE] {
        let ours = fs.read_inode(ino_of(&fs, name)).unwrap();
        let theirs = fs.read_inode(ino_of(&fs, &control(name))).unwrap();
        assert_eq!(
            (ours.size, ours.nbytes),
            (theirs.size, theirs.nbytes),
            "{name}'s size and nbytes are not what the kernel's own write gave its control"
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

    let names = [
        GAP,
        NEAR,
        STALE,
        "gap-control.bin",
        "near-control.bin",
        "stale-control.bin",
    ];
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
    for name in [GAP, NEAR, STALE] {
        assert_eq!(
            stats[name],
            stats[&control(name)],
            "the kernel's size and allocation of {name} differ from its control's"
        );
        assert_eq!(
            after[name].0,
            sha256_hex(&expected[name]),
            "the kernel reads back different bytes from {name} than were written: \
             the gap and the old sector's tail must read as zeros"
        );
        assert_eq!(
            after[name].0,
            after[&control(name)].0,
            "{name} does not read as the kernel's own write left its control"
        );
    }
}
