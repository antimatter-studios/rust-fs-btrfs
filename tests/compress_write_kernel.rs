//! Copy-on-write writes into files that ask for compression, judged by
//! what is not this crate (#265).
//!
//! A kernel-made volume holds files the kernel wrote uncompressed and then
//! marked: `chattr +c`, the compression property naming LZO or zstd,
//! `chattr +m` (no compression), or nothing. This crate overwrites each
//! with compressible bytes or with noise, and each compressed extent is
//! overwritten again — replacing the compressed extent just written. A
//! second volume, whose files ask for nothing, is written through a mount
//! set to compress, first with LZO and then with zstd, so the first
//! extent of each is this crate's and so is the feature flag it needs.
//!
//! The judges: `btrfs inspect-internal dump-tree` names each extent's
//! compression type, `dump-super` the volume's feature flags, `filefrag`
//! asks the kernel which extents are encoded, `btrfs check` and `btrfs
//! check --check-data-csum` (every on-disk sector against its digest)
//! read the volume, the kernel's reads give back each file's bytes, and a
//! kernel scrub reads every extent again.
//!
//! A third volume holds files the kernel wrote as one extent several
//! hundred KiB long. A copy of one is longer than a compression unit, and
//! is stored compressed as several extents of at most 128 KiB each, as
//! the kernel stores a long range (#357).

use fs_btrfs::compression::Compression;
use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_super, dump_tree, guest_kernel_read_ok, guest_kernel_write_ok,
    oracle, sha256_hex, temp_path,
};
use fs_core::{BlockDevice, FileDevice};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// How long each file is: one extent, well under a compression unit.
const FILE_LEN: usize = 65536;

/// The most a compressed extent may decode to (`BTRFS_MAX_UNCOMPRESSED`).
const COMPRESSION_UNIT: u64 = 128 * 1024;

/// How long each long file is: three compression units and five sectors,
/// so a copy of it is three whole units and a short one.
const LONG_LEN: usize = 3 * 131_072 + 20_480;

/// `BTRFS_FEATURE_INCOMPAT_COMPRESS_LZO` and `..._COMPRESS_ZSTD`.
const INCOMPAT_COMPRESS_LZO: u64 = 1 << 3;
const INCOMPAT_COMPRESS_ZSTD: u64 = 1 << 4;

/// A scratch volume, removed when it drops — including on a panic.
struct Scratch(PathBuf);

impl Scratch {
    /// A fresh volume holding `files`, each 64 KiB the kernel wrote, then
    /// `marks` run in the mount's directory.
    fn new(name: &str, files: &[&str], marks: &str) -> Self {
        Self::with_len(name, files, FILE_LEN, marks)
    }

    /// As [`Scratch::new`], each file `len` bytes long.
    fn with_len(name: &str, files: &[&str], len: usize, marks: &str) -> Self {
        let dir = PathBuf::from(temp_path!("{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("fs.img");
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(256 << 20))
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
        let mut script = String::from("cd \"$MNT\"\n");
        for file in files {
            script.push_str(&format!("head -c {len} /dev/urandom > {file}\n"));
        }
        script.push_str("sync\n");
        script.push_str(marks);
        script.push_str("\nsync");
        guest_kernel_write_ok(&image.to_string_lossy(), name, &script);
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

/// Text a compressor shrinks several times over.
fn text() -> Vec<u8> {
    text_of(FILE_LEN)
}

/// `len` bytes of that text.
fn text_of(len: usize) -> Vec<u8> {
    let line = b"a line of very compressible text, ";
    line.repeat(len / line.len() + 1)[..len].to_vec()
}

/// Bytes no compressor shrinks: xorshift64 output.
fn noise() -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut out = Vec::with_capacity(FILE_LEN);
    while out.len() < FILE_LEN {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out
}

/// Every regular file extent in `dump` (`dump-tree` of the fs tree): its
/// inode, and the compression byte its item records.
fn extent_compression(dump: &str) -> BTreeMap<u64, Vec<u8>> {
    let mut out: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let mut inode = None;
    for line in dump.lines() {
        if let Some(key) = line.split("key (").nth(1) {
            inode = key
                .contains(" EXTENT_DATA ")
                .then(|| key.split(' ').next().and_then(|n| n.parse::<u64>().ok()))
                .flatten();
            continue;
        }
        let (Some(ino), Some(rest)) = (inode, line.split("compression ").nth(1)) else {
            continue;
        };
        let byte = rest
            .split(' ')
            .next()
            .and_then(|n| n.parse::<u8>().ok())
            .unwrap_or_else(|| {
                panic!("dump-tree printed a compression that is not a byte: {line}")
            });
        out.entry(ino).or_default().push(byte);
    }
    out
}

/// Every regular file extent in `dump` (`dump-tree` of the fs tree): its
/// inode, and how much of the file its item covers (`num_bytes`, which
/// `dump-tree` prints as the `nr` after `extent data offset`).
fn extent_lengths(dump: &str) -> BTreeMap<u64, Vec<u64>> {
    let mut out: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    let mut inode = None;
    for line in dump.lines() {
        if let Some(key) = line.split("key (").nth(1) {
            inode = key
                .contains(" EXTENT_DATA ")
                .then(|| key.split(' ').next().and_then(|n| n.parse::<u64>().ok()))
                .flatten();
            continue;
        }
        let (Some(ino), Some(rest)) = (inode, line.split("extent data offset ").nth(1)) else {
            continue;
        };
        let len = rest
            .split(" nr ")
            .nth(1)
            .and_then(|n| n.split(' ').next())
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or_else(|| panic!("dump-tree printed an extent with no length: {line}"));
        out.entry(ino).or_default().push(len);
    }
    out
}

/// The volume's incompatible feature flags, as `dump-super` prints them.
fn incompat_flags(image: &Path) -> u64 {
    let dump = dump_super(image);
    dump.lines()
        .find_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some("incompat_flags"))
                .then(|| words.next())
                .flatten()
                .and_then(|hex| u64::from_str_radix(hex.trim_start_matches("0x"), 16).ok())
        })
        .unwrap_or_else(|| panic!("dump-super printed no incompat_flags:\n{dump}"))
}

/// What a file should hold, and how its extent should be stored.
struct Case {
    name: &'static str,
    bytes: Vec<u8>,
    /// The compression byte every extent of the file records.
    compression: u8,
}

/// The kernel's verdict on `cases`: each file's bytes read back through
/// the kernel, encoded extents where (and only where) compression was
/// expected, every extent item's compression type as `dump-tree` prints
/// it, `btrfs check` with and without the data digests, and a scrub.
fn judge(image: &Path, fs_inodes: &BTreeMap<&str, u64>, cases: &[Case]) {
    let dump = dump_tree(image, "5");
    let types = extent_compression(&dump);
    for case in cases {
        let ino = fs_inodes[case.name];
        let seen = types
            .get(&ino)
            .unwrap_or_else(|| panic!("dump-tree shows no extent for {}:\n{dump}", case.name));
        assert!(
            seen.iter().all(|&c| c == case.compression),
            "{} (inode {ino}) has extents of compression {seen:?}, expected {}:\n{dump}",
            case.name,
            case.compression
        );
    }

    assert_btrfs_check_clean(image, "after writes into files asking for compression");
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(image)
        .output();
    assert_eq!(
        checked.status.code(),
        Some(0),
        "btrfs check --check-data-csum finds data that disagrees with its digests:\n{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );

    let names: Vec<&str> = cases.iter().map(|c| c.name).collect();
    let seen = guest_kernel_read_ok(
        &image.to_string_lossy(),
        "after the writes",
        &format!(
            "cd \"$MNT\"\n\
             for f in {}; do \
               printf '%s %s %s\\n' \"$f\" \"$(sha256sum \"$f\" | cut -d' ' -f1)\" \
                 \"$(filefrag -v \"$f\" | grep -c encoded || true)\"; done",
            names.join(" ")
        ),
    );
    let lines: Vec<Vec<&str>> = seen.lines().map(|l| l.split(' ').collect()).collect();
    assert_eq!(lines.len(), cases.len(), "the kernel reported:\n{seen}");
    for (case, line) in cases.iter().zip(&lines) {
        assert_eq!(line[0], case.name, "{seen}");
        assert_eq!(
            line[1],
            sha256_hex(&case.bytes),
            "the kernel reads back different bytes from {}:\n{seen}",
            case.name
        );
        if case.compression == 0 {
            assert_eq!(line[2], "0", "{} is stored compressed:\n{seen}", case.name);
        } else {
            assert_ne!(
                line[2], "0",
                "{} has no extent the kernel calls encoded:\n{seen}",
                case.name
            );
        }
    }

    guest_kernel_write_ok(
        &image.to_string_lossy(),
        "scrub",
        "btrfs scrub start -B \"$MNT\" >/dev/null",
    );
}

/// Write each case's bytes over its whole file, then, into each that is
/// compressed, 1000 bytes again: the second write replaces the compressed
/// extent the first one made. Returns each file's inode.
fn write_cases(fs: &mut Filesystem, cases: &mut [Case]) -> BTreeMap<&'static str, u64> {
    let mut inodes = BTreeMap::new();
    for case in cases.iter_mut() {
        let ino = fs.lookup_path(&format!("/{}", case.name)).unwrap().ino;
        inodes.insert(case.name, ino);
        fs.write(ino, 0, &case.bytes)
            .unwrap_or_else(|e| panic!("writing {}: {e}", case.name));
        if case.compression != 0 {
            fs.write(ino, 30_000, &[0x5a; 1000])
                .unwrap_or_else(|e| panic!("overwriting {}'s compressed extent: {e}", case.name));
            case.bytes[30_000..31_000].copy_from_slice(&[0x5a; 1000]);
        }
    }
    for case in cases.iter() {
        assert_eq!(
            fs.read_file(inodes[case.name]).unwrap(),
            case.bytes,
            "{} reads back differently through this driver",
            case.name
        );
    }
    inodes
}

/// Each file's own choice: `chattr +c` is zlib, the property names LZO or
/// zstd, and a file that asks for nothing, or whose bytes do not shrink by
/// a sector, is stored as it is.
#[test]
fn a_file_asking_for_compression_is_written_in_its_algorithm_and_the_kernel_reads_it() {
    let files = [
        "zlib.bin",
        "lzo.bin",
        "zstd.bin",
        "plain.bin",
        "noise-zlib.bin",
        "noise-lzo.bin",
        "noise-zstd.bin",
    ];
    let scratch = Scratch::new(
        "compress-write",
        &files,
        "chattr +c zlib.bin noise-zlib.bin\n\
         btrfs property set lzo.bin compression lzo\n\
         btrfs property set noise-lzo.bin compression lzo\n\
         btrfs property set zstd.bin compression zstd\n\
         btrfs property set noise-zstd.bin compression zstd",
    );
    let image = scratch.image();

    let mut cases = vec![
        Case {
            name: "zlib.bin",
            bytes: text(),
            compression: 1,
        },
        Case {
            name: "lzo.bin",
            bytes: text(),
            compression: 2,
        },
        Case {
            name: "zstd.bin",
            bytes: text(),
            compression: 3,
        },
        Case {
            name: "plain.bin",
            bytes: text(),
            compression: 0,
        },
        Case {
            name: "noise-zlib.bin",
            bytes: noise(),
            compression: 0,
        },
        Case {
            name: "noise-lzo.bin",
            bytes: noise(),
            compression: 0,
        },
        Case {
            name: "noise-zstd.bin",
            bytes: noise(),
            compression: 0,
        },
    ];
    let mut fs = mount_rw(&image);
    let inodes = write_cases(&mut fs, &mut cases);
    drop(fs);

    judge(&image, &inodes, &cases);
}

/// A mount set to compress compresses files that ask for nothing, but not
/// one that declines (`chattr +m`). The volume has used neither LZO nor
/// zstd before, so the first extent of each is this crate's, and so is
/// the incompatible feature flag announcing it — set in the commit that
/// writes it, and not before.
#[test]
fn a_mount_set_to_compress_writes_lzo_and_zstd_and_sets_their_feature_flags() {
    let files = ["by-lzo.bin", "by-zstd.bin", "declined.bin"];
    let scratch = Scratch::new("compress-mount", &files, "chattr +m declined.bin");
    let image = scratch.image();
    let both = INCOMPAT_COMPRESS_LZO | INCOMPAT_COMPRESS_ZSTD;
    assert_eq!(
        incompat_flags(&image) & both,
        0,
        "mkfs.btrfs already set a compression flag, so this would test nothing"
    );

    let mut lzo = vec![
        Case {
            name: "by-lzo.bin",
            bytes: text(),
            compression: 2,
        },
        Case {
            name: "declined.bin",
            bytes: text(),
            compression: 0,
        },
    ];
    let mut fs = mount_rw(&image);
    fs.set_compression(Compression::Lzo);
    let mut inodes = write_cases(&mut fs, &mut lzo);
    assert_eq!(
        fs.compression(),
        Compression::Lzo,
        "the mount forgot its compression across the commits"
    );
    drop(fs);
    assert_eq!(
        incompat_flags(&image) & both,
        INCOMPAT_COMPRESS_LZO,
        "after the first LZO extent, the volume should announce LZO and only LZO"
    );

    let mut zstd = vec![Case {
        name: "by-zstd.bin",
        bytes: text(),
        compression: 3,
    }];
    let mut fs = mount_rw(&image);
    fs.set_compression(Compression::Zstd);
    inodes.extend(write_cases(&mut fs, &mut zstd));
    drop(fs);
    assert_eq!(
        incompat_flags(&image) & both,
        both,
        "after the first zstd extent, the volume should announce both algorithms"
    );

    lzo.extend(zstd);
    judge(&image, &inodes, &lzo);
}

/// A copy longer than one compression unit is stored compressed as
/// several extents of at most 128 KiB each, one item per extent, as the
/// kernel stores a long range (#357). Each file was written by the kernel
/// uncompressed, as one extent several hundred KiB long, and then asked
/// for zlib (`chattr +c`), LZO or zstd (the compression property).
#[test]
fn a_copy_longer_than_a_compression_unit_is_compressed_in_units() {
    let files = ["long-zlib.bin", "long-lzo.bin", "long-zstd.bin"];
    let scratch = Scratch::with_len(
        "compress-long",
        &files,
        LONG_LEN,
        "chattr +c long-zlib.bin\n\
         btrfs property set long-lzo.bin compression lzo\n\
         btrfs property set long-zstd.bin compression zstd",
    );
    let image = scratch.image();

    let mut fs = mount_rw(&image);
    let before = extent_lengths(&dump_tree(&image, "5"));
    for name in files {
        let ino = fs.lookup_path(&format!("/{name}")).unwrap().ino;
        let lengths = before.get(&ino).cloned().unwrap_or_default();
        assert!(
            lengths.iter().any(|&len| len > COMPRESSION_UNIT),
            "the kernel wrote {name} as extents of {lengths:?} bytes, none longer than a \
             compression unit, so its copy would not be either"
        );
    }

    let mut cases: Vec<Case> = [
        ("long-zlib.bin", 1),
        ("long-lzo.bin", 2),
        ("long-zstd.bin", 3),
    ]
    .into_iter()
    .map(|(name, compression)| Case {
        name,
        bytes: text_of(LONG_LEN),
        compression,
    })
    .collect();
    let inodes = write_cases(&mut fs, &mut cases);
    drop(fs);

    let after = extent_lengths(&dump_tree(&image, "5"));
    for case in &cases {
        let lengths = after.get(&inodes[case.name]).cloned().unwrap_or_default();
        assert!(
            lengths.len() >= 4 && lengths.iter().all(|&len| len <= COMPRESSION_UNIT),
            "{} is stored as extents of {lengths:?} bytes; a copy of {LONG_LEN} bytes asking \
             for compression is four extents of at most {COMPRESSION_UNIT}",
            case.name
        );
        assert_eq!(
            lengths.iter().sum::<u64>(),
            LONG_LEN as u64,
            "{}'s extents do not cover the file exactly once: {lengths:?}",
            case.name
        );
    }

    judge(&image, &inodes, &cases);
}
