//! Ranges deduplicated through the C ABI, judged by what is not this crate
//! (#712).
//!
//! The kernel makes a volume of twin files, each pair holding the same
//! recipe of bytes, and each case below dedupes one twin through the ABI
//! and the other through the kernel's own `FIDEDUPERANGE`. Then:
//!
//! - the ABI's status and byte count must be the kernel's, and a range the
//!   kernel refuses must be refused;
//! - `btrfs check --check-data-csum` inspects the whole volume;
//! - `dump-tree` must show the same extent items and the same data
//!   references for both twins, and ranges that differ must change nothing;
//! - no file's bytes move, and neither do a destination's change and
//!   modification times, for either twin;
//! - and the kernel writes to and deletes the files afterwards, which finds
//!   and drops each reference this crate added.

#![allow(unsafe_code)]

use fs_btrfs::capi::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, guest_kernel_read_ok, guest_kernel_write_ok, oracle,
    sha256_hex, temp_path,
};
use std::collections::BTreeMap;
use std::ffi::{c_void, CStr, CString};
use std::path::{Path, PathBuf};

const ENOENT: i32 = 2;
const EISDIR: i32 = 21;
const EROFS: i32 = 30;
const ENOTSUP: i32 = if cfg!(target_os = "macos") { 45 } else { 95 };

/// One dedupe: the source and its offset, the file the ABI dedupes, its
/// twin the kernel dedupes, the destination offset, the length asked for,
/// and what the kernel reports: the destination's status and bytes, or -1
/// and an errno when the whole call fails. A negative status means the ABI
/// must refuse the range.
struct Case {
    src: &'static str,
    src_offset: u64,
    ours: &'static str,
    twin: &'static str,
    dst_offset: u64,
    len: u64,
    status: i32,
    bytes: u64,
}

const fn case(
    src: &'static str,
    src_offset: u64,
    (ours, twin): (&'static str, &'static str),
    dst_offset: u64,
    len: u64,
    (status, bytes): (i32, u64),
) -> Case {
    Case {
        src,
        src_offset,
        ours,
        twin,
        dst_offset,
        len,
        status,
        bytes,
    }
}

const EINVAL: u64 = 22;

const CASES: [Case; 10] = [
    // A whole file, across both of the source's extents.
    case("src", 0, ("d1", "k1"), 0, 131_072, (0, 131_072)),
    // Across the source's extent boundary, in the middle of the destination.
    case("src", 61_440, ("d2", "k2"), 61_440, 8192, (0, 8192)),
    // At another offset in the destination, to both files' ends.
    case("src", 0, ("d3", "k3"), 4096, 131_072, (0, 131_072)),
    // A length that is not whole sectors and reaches no end: one sector is
    // shared, and the length asked for is reported.
    case("src", 0, ("d4", "k4"), 0, 5000, (0, 5000)),
    // A range past the source's end fails the whole call.
    case("src", 65_536, ("d5", "k5"), 65_536, 1 << 20, (-1, EINVAL)),
    // Both ranges end in the same partial last sector.
    case("odd", 0, ("do1", "ko1"), 0, 10_000, (0, 10_000)),
    // The source's end is not the destination's, so whole sectors only.
    case("odd", 0, ("do2", "ko2"), 0, 10_000, (0, 10_000)),
    // A range past the destination's end fails that destination.
    case("src", 0, ("ds", "ks"), 0, 131_072, (-22, 0)),
    // Ranges that differ near their end: nothing is written.
    case("src", 0, ("dx", "kx"), 0, 131_072, (1, 0)),
    // Less than one sector, reaching no end: nothing to share.
    case("src", 0, ("d6", "k6"), 0, 100, (0, 100)),
];

/// The kernel's twin files: `d*` for the ABI and `k*` for the kernel, each
/// written with `dd`, because `cp` and `cat` may reflink, and sharing
/// extents before the dedupe would leave it nothing to do.
const MAKE: &str = r#"cd "$MNT"
head -c 65536 /dev/urandom > src
sync
head -c 65536 /dev/urandom >> src
sync
head -c 10000 /dev/urandom > odd
sync
copy() { dd if="$1" of="$2" bs=1M status=none; }
for p in d k; do
  copy src ${p}1
  copy src ${p}2
  { head -c 4096 /dev/urandom; dd if=src bs=1M status=none; } \
    | dd of=${p}3 bs=1M iflag=fullblock status=none
  copy src ${p}4
  copy src ${p}5
  copy src ${p}6
  copy odd ${p}o1
  { dd if=odd bs=1M status=none; head -c 10000 /dev/urandom; } \
    | dd of=${p}o2 bs=1M iflag=fullblock status=none
  dd if=src of=${p}s bs=70000 count=1 status=none
  copy src ${p}x
  head -c 4096 /dev/urandom | dd of=${p}x bs=4096 seek=31 conv=notrunc status=none
done
sync"#;

fn last_error() -> String {
    unsafe { CStr::from_ptr(fs_btrfs_last_error()) }
        .to_string_lossy()
        .into_owned()
}

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

/// A scratch volume, removed when it drops, including on a panic.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = PathBuf::from(temp_path!("{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("fs.img");
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(256 << 20))
            .unwrap();
        let made = oracle("mkfs.btrfs")
            .args(["-q", "-f", "-s", "4096", "-n", "16384", "-O", "no-holes"])
            .arg(&image)
            .output();
        assert!(
            made.status.success(),
            "mkfs.btrfs: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        guest_kernel_write_ok(&image.to_string_lossy(), name, MAKE);
        Scratch(dir)
    }

    fn image(&self) -> String {
        self.0.join("fs.img").to_string_lossy().into_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The status of a dedupe and the bytes it reported.
fn dedupe(
    fs: *mut fs_btrfs_fs,
    src: &str,
    src_offset: u64,
    dst: &str,
    dst_offset: u64,
    len: u64,
) -> (i32, u64) {
    let mut bytes = u64::MAX;
    let status = unsafe {
        fs_btrfs_dedupe_range(
            fs,
            c(src).as_ptr(),
            src_offset,
            c(dst).as_ptr(),
            dst_offset,
            len,
            &mut bytes,
        )
    };
    (status, bytes)
}

#[track_caller]
fn refused(fs: *mut fs_btrfs_fs, src: &str, at: u64, dst: &str, to: u64, len: u64, want: i32) {
    let (status, _) = dedupe(fs, src, at, dst, to, len);
    assert_eq!(
        status, -1,
        "dedupe {src} {at} against {dst} {to} ({len}) succeeded and should not have"
    );
    assert_eq!(
        fs_btrfs_last_errno(),
        want,
        "dedupe {src} {at} against {dst} {to} ({len}): wrong errno for {:?}",
        last_error()
    );
}

fn read(fs: *mut fs_btrfs_fs, path: &str) -> Vec<u8> {
    let mut buf = vec![0u8; 1 << 20];
    let n = unsafe {
        fs_btrfs_read_file(
            fs,
            c(path).as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            0,
            buf.len() as u64,
        )
    };
    assert!(n >= 0, "read {path}: {}", last_error());
    buf.truncate(n as usize);
    buf
}

/// The `(objectid TYPE offset)` of an `item N key (...)` line.
fn item_key(line: &str) -> Option<(u64, String, u64)> {
    let rest = line.trim_start().strip_prefix("item ")?;
    let (_, key) = rest.split_once("key (")?;
    let (key, _) = key.split_once(')')?;
    let mut words = key.split_whitespace();
    let objectid = words.next()?.parse().ok()?;
    let kind = words.next()?.to_string();
    let offset = words.next()?.parse().ok()?;
    Some((objectid, kind, offset))
}

fn after(line: &str, word: &str) -> u64 {
    let mut it = line.split_whitespace();
    while let Some(w) = it.next() {
        if w == word {
            return it.next().unwrap().parse().unwrap();
        }
    }
    panic!("no {word} in {line:?}")
}

/// Inode `ino`'s extent items as key, disk address, disk length, offset
/// within the extent and length.
fn extent_items(fs_tree: &str, ino: u64) -> Vec<[u64; 5]> {
    let lines: Vec<&str> = fs_tree.lines().collect();
    let mut items = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        let Some((objectid, kind, key_offset)) = item_key(line) else {
            continue;
        };
        if objectid != ino || kind != "EXTENT_DATA" {
            continue;
        }
        let body: Vec<&str> = lines[at + 1..]
            .iter()
            .take_while(|l| item_key(l).is_none())
            .copied()
            .collect();
        let disk = body
            .iter()
            .find(|l| l.contains("extent data disk byte"))
            .unwrap_or_else(|| panic!("inode {ino}'s item at {key_offset} has no disk byte"));
        let within = body
            .iter()
            .find(|l| l.contains("extent data offset"))
            .unwrap_or_else(|| panic!("inode {ino}'s item at {key_offset} has no offset"));
        items.push([
            key_offset,
            after(disk, "byte"),
            after(disk, "nr"),
            after(within, "offset"),
            after(within, "nr"),
        ]);
    }
    items
}

/// Inode `ino`'s extent items with each extent named by what it is: `src N`
/// for the sources' `N`th, at one address for both twins, and `own N` for
/// the `N`th of the file's own, at a different address in each.
fn item_shapes(fs_tree: &str, ino: u64, source: &[u64]) -> Vec<(u64, String, u64, u64, u64)> {
    let mut own: Vec<u64> = Vec::new();
    extent_items(fs_tree, ino)
        .into_iter()
        .map(|[key, bytenr, disk_len, within, len]| {
            let name = match source.iter().position(|b| *b == bytenr) {
                Some(n) => format!("src {n}"),
                None => {
                    let n = own.iter().position(|b| *b == bytenr).unwrap_or_else(|| {
                        own.push(bytenr);
                        own.len() - 1
                    });
                    format!("own {n}")
                }
            };
            (key, name, disk_len, within, len)
        })
        .collect()
}

/// The offset and count of every data reference naming inode `ino`, inline
/// or an item of its own: `dump-tree` prints both forms alike.
fn references_naming(extent_tree: &str, ino: u64) -> Vec<(u64, u64)> {
    let mut found: Vec<(u64, u64)> = extent_tree
        .lines()
        .filter(|l| l.contains("extent data backref root"))
        .filter(|l| after(l, "objectid") == ino)
        .map(|l| (after(l, "offset"), after(l, "count")))
        .collect();
    found.sort_unstable();
    found
}

/// The kernel's `FIDEDUPERANGE` for every case's twin, then each file's
/// inode. `0xc0189436` is `_IOWR(0x94, 54, struct file_dedupe_range)`; the
/// argument is that header and one `struct file_dedupe_range_info`.
fn kernel_dedupes() -> String {
    let calls: String = CASES
        .iter()
        .map(|k| {
            format!(
                "    ('{}', {}, '{}', {}, {}),\n",
                k.src, k.src_offset, k.twin, k.dst_offset, k.len
            )
        })
        .collect();
    format!(
        "cd \"$MNT\"\n\
         results=$(python3 - <<'PY'\n\
         import fcntl, os, struct\n\
         FIDEDUPERANGE = 0xc0189436\n\
         SHAPE = '=QQHHIqQQiI'\n\
         for src, src_offset, dst, dst_offset, length in [\n{calls}]:\n\
         \x20   s = os.open(src, os.O_RDONLY)\n\
         \x20   d = os.open(dst, os.O_RDWR)\n\
         \x20   arg = bytearray(struct.pack(SHAPE, src_offset, length, 1, 0, 0, d, dst_offset, 0, 0, 0))\n\
         \x20   try:\n\
         \x20       fcntl.ioctl(s, FIDEDUPERANGE, arg)\n\
         \x20       out = struct.unpack(SHAPE, arg)\n\
         \x20       print(dst, out[8], out[7])\n\
         \x20   except OSError as e:\n\
         \x20       print(dst, -1, e.errno)\n\
         \x20   os.close(s)\n\
         \x20   os.close(d)\n\
         PY\n\
         )\n\
         sync\n\
         printf '%s\\n' \"$results\"\n"
    )
}

/// Every file's name, size and SHA-256, as the kernel reads them.
fn kernel_reads(image: &str, what: &str, names: &[&str]) -> String {
    guest_kernel_read_ok(
        image,
        what,
        &format!(
            "cd \"$MNT\"\n\
             for f in {all}; do \
               printf '%s %s %s\\n' \"$f\" \"$(stat -c %s \"$f\")\" \
                 \"$(sha256sum \"$f\" | cut -d' ' -f1)\"; done",
            all = names.join(" ")
        ),
    )
}

/// Each destination's modification and change times, as the kernel reads
/// them.
fn times(image: &str, what: &str) -> String {
    let names: Vec<&str> = CASES.iter().flat_map(|k| [k.ours, k.twin]).collect();
    guest_kernel_read_ok(
        image,
        what,
        &format!("cd \"$MNT\"\nstat -c '%n %y %z' {}", names.join(" ")),
    )
}

#[test]
fn ranges_deduped_through_the_abi_share_their_extents_as_the_kernel_shares_them() {
    let scratch = Scratch::new("dedupe-kernel");
    let image = scratch.image();
    let times_before = times(&image, "before the dedupes");

    let ro = unsafe { fs_btrfs_mount(c(&image).as_ptr()) };
    assert!(!ro.is_null(), "mount: {}", last_error());
    refused(ro, "/src", 0, "/d1", 0, 4096, EROFS);
    unsafe { fs_btrfs_umount(ro) };

    let fs = unsafe { fs_btrfs_mount_rw(c(&image).as_ptr()) };
    assert!(!fs.is_null(), "mount_rw: {}", last_error());

    let mut names: Vec<&str> = vec!["src", "odd"];
    names.extend(CASES.iter().flat_map(|k| [k.ours, k.twin]));
    let want: BTreeMap<&str, Vec<u8>> = names
        .iter()
        .map(|name| (*name, read(fs, &format!("/{name}"))))
        .collect();
    assert_eq!(want["src"].len(), 131_072, "/src is not the size made");
    assert_eq!(want["ds"].len(), 70_000, "/ds is not the size made");
    assert_eq!(want["dx"].len(), 131_072, "/dx is not the size made");
    assert_ne!(want["src"], want["dx"], "/dx does not differ from /src");
    assert_eq!(
        want["src"][..4096],
        want["d4"][..4096],
        "/d4 does not begin as /src begins"
    );

    // What the kernel refuses is refused, and a miss names what is missing.
    refused(fs, "/src", 1, "/d1", 0, 4096, ENOTSUP);
    refused(fs, "/src", 0, "/d1", 1, 4096, ENOTSUP);
    refused(fs, "/src", 131_072, "/d1", 0, 4096, ENOTSUP);
    refused(fs, "/src", 0, "/d4", 131_072, 4096, ENOTSUP);
    refused(fs, "/src", 0, "/src", 4096, 8192, ENOTSUP);
    refused(fs, "/missing", 0, "/d1", 0, 4096, ENOENT);
    refused(fs, "/src", 0, "/missing", 0, 4096, ENOENT);
    refused(fs, "/", 0, "/d1", 0, 4096, EISDIR);
    refused(fs, "/src", 0, "/", 0, 4096, EISDIR);

    // A length of zero shares nothing and reports nothing.
    assert_eq!(dedupe(fs, "/src", 0, "/d1", 0, 0), (0, 0), "a zero length");

    for k in &CASES {
        let (src, ours) = (format!("/{}", k.src), format!("/{}", k.ours));
        if k.status < 0 {
            refused(fs, &src, k.src_offset, &ours, k.dst_offset, k.len, ENOTSUP);
            continue;
        }
        let got = dedupe(fs, &src, k.src_offset, &ours, k.dst_offset, k.len);
        assert_eq!(
            got,
            (k.status, k.bytes),
            "dedupe of {ours} against {src}: {}",
            last_error()
        );
    }
    for (name, bytes) in &want {
        assert!(
            read(fs, &format!("/{name}")) == *bytes,
            "/{name} changed its bytes in a dedupe"
        );
    }
    unsafe { fs_btrfs_umount(fs) };

    // The checker: each released extent's record, free space and checksums
    // gone, each shared extent counted, and every byte count right.
    assert_btrfs_check_clean(
        Path::new(&image),
        "after ranges were deduped through the ABI",
    );
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

    // What the kernel reads.
    let expected: String = names
        .iter()
        .map(|name| {
            let bytes = &want[name];
            format!("{name} {} {}\n", bytes.len(), sha256_hex(bytes))
        })
        .collect();
    assert_eq!(
        kernel_reads(&image, "after the ABI's dedupes", &names),
        expected,
        "the kernel reads the deduped files differently"
    );
    guest_kernel_write_ok(&image, "scrub", "btrfs scrub start -B \"$MNT\" >/dev/null");

    // The kernel dedupes the twins, and must report what the ABI reported.
    let reported = guest_kernel_write_ok(&image, "the kernel's own dedupes", &kernel_dedupes());
    let wanted: String = CASES
        .iter()
        .map(|k| format!("{} {} {}\n", k.twin, k.status, k.bytes))
        .collect();
    assert_eq!(reported, wanted, "the kernel reported other results");

    // Neither the ABI's dedupes nor the kernel's moved a destination's times.
    assert_eq!(
        times(&image, "after the dedupes"),
        times_before,
        "a dedupe moved a destination's modification or change time"
    );

    // The kernel's items and references must be the ones written here.
    let inodes = guest_kernel_read_ok(
        &image,
        "each file's inode",
        &format!(
            "cd \"$MNT\"\nfor f in {}; do printf '%s %s\\n' \"$f\" \"$(stat -c %i \"$f\")\"; done",
            names.join(" ")
        ),
    );
    let ino: BTreeMap<&str, u64> = inodes
        .lines()
        .map(|line| {
            let (name, n) = line.split_once(' ').unwrap();
            (name, n.parse().unwrap())
        })
        .collect();
    let fs_tree = dump_tree(Path::new(&image), "5");
    let extents = dump_tree(Path::new(&image), "extent");
    let source: Vec<u64> = ["src", "odd"]
        .iter()
        .flat_map(|name| extent_items(&fs_tree, ino[name]))
        .map(|item| item[1])
        .collect();
    assert_eq!(source.len(), 3, "/src and /odd are not three extents");
    for k in &CASES {
        let mine = item_shapes(&fs_tree, ino[k.ours], &source);
        assert_eq!(
            mine.iter().any(|(_, name, ..)| name.starts_with("src")),
            k.status == 0 && k.len >= 4096,
            "/{} shares the wrong extents: {mine:?}",
            k.ours
        );
        assert_eq!(
            mine,
            item_shapes(&fs_tree, ino[k.twin], &source),
            "/{}'s extent items are not the ones the kernel made for /{}",
            k.ours,
            k.twin
        );
        assert_eq!(
            references_naming(&extents, ino[k.ours]),
            references_naming(&extents, ino[k.twin]),
            "the references naming /{} are not the ones naming /{}",
            k.ours,
            k.twin
        );
    }

    // The kernel deletes the sources and writes into what shared them: each
    // reference added or dropped here is one it has to find.
    guest_kernel_write_ok(
        &image,
        "the kernel changes the deduped files",
        "cd \"$MNT\"\n\
         rm src d2\n\
         head -c 4096 /dev/zero | tr '\\0' x | dd of=d1 bs=4096 seek=4 conv=notrunc status=none\n\
         sync",
    );
    assert_btrfs_check_clean(
        Path::new(&image),
        "after the kernel changed the deduped files",
    );
    let mut left = want.clone();
    left.remove("src");
    left.remove("d2");
    left.get_mut("d1").unwrap()[16_384..20_480].copy_from_slice(&[b'x'; 4096]);
    let names: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| left.contains_key(name))
        .collect();
    let expected: String = names
        .iter()
        .map(|name| {
            let bytes = &left[name];
            format!("{name} {} {}\n", bytes.len(), sha256_hex(bytes))
        })
        .collect();
    assert_eq!(
        kernel_reads(&image, "after the kernel's changes", &names),
        expected,
        "the kernel reads the deduped files differently after changing them"
    );
}
