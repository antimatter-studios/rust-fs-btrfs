//! Devices removed through `Filesystem::remove_device`, judged by what is
//! not this crate (#303).
//!
//! # What a remove has to get right
//!
//! A device leaves in one committed transaction: its `DEV_ITEM` gone from
//! the chunk tree, `num_devices` one fewer and `total_bytes` its size less
//! in every other member's superblock, and then its own superblocks wiped.
//! Leave the item behind and the kernel waits for a device that is not
//! coming; leave a count wrong and it will not assemble the pool; leave
//! the device's superblock and a scan finds a member nobody counts.
//!
//! # The oracles
//!
//! The kernel makes a volume and writes its files, and a manifest of them
//! is taken then. This crate adds two devices to it, removes the first of
//! them, and later the second. After each removal, in the harness VM with
//! only the devices left attached: `btrfs check --readonly`, `btrfs
//! filesystem show` and `btrfs filesystem usage` for the devices and the
//! size the kernel believes in, and the kernel reading every file of the
//! manifest. With two devices left the kernel also fills the pool past what
//! the first device holds, which it can only do on the other one, and
//! `btrfs check` looks again.

use fs_btrfs::error::Error;
use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{
    fixture, guest_kernel_pool_read, guest_kernel_pool_write, guest_kernel_write_ok, oracle,
    sha256_hex, temp_path, PoolRead,
};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MIB: u64 = 1 << 20;

/// The volume the kernel makes, and the two devices added to it.
const SIZES: [u64; 3] = [256 * MIB, 256 * MIB, 128 * MIB];

/// A scratch directory of images, removed when it drops — including on a
/// panic.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        // In the repository's scratch directory: the harness VM sees this
        // repository and nothing else of the host.
        let dir = PathBuf::from(temp_path!(
            "btrfs-device-remove-{name}-{}-{:?}",
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

    /// A copy of the fixture `name`.
    fn copy(&self, name: &str) -> PathBuf {
        let src = fixture(name);
        let image = self.0.join(name);
        std::fs::copy(&src, &image).unwrap_or_else(|e| panic!("copying {}: {e}", src.display()));
        image
    }

    /// An image of `size` bytes holding a filesystem `mkfs.btrfs` made.
    fn formatted(&self, name: &str, size: u64) -> PathBuf {
        let image = self.blank(name, size);
        let made = oracle("mkfs.btrfs")
            .args(["-q", "-f", "-s", "4096", "-n", "16384"])
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

/// `path -> (size, sha256)` from lines of `path\tsize\tsha256`, each line
/// starting with `tag\t` when a tag is given.
fn manifest(text: &str, tag: Option<&str>) -> BTreeMap<String, (String, String)> {
    text.lines()
        .filter_map(|line| {
            let mut f: Vec<&str> = line.split('\t').collect();
            if let Some(tag) = tag {
                if f.first() != Some(&tag) {
                    return None;
                }
                f.remove(0);
            }
            (f.len() == 3).then(|| (f[0].to_string(), (f[1].to_string(), f[2].to_string())))
        })
        .collect()
}

/// Every file under `$MNT`, as `[tag\t]path\tsize\tsha256`.
fn list_files(tag: &str) -> String {
    format!(
        "(cd \"$MNT\" && find . -type f | sort | while read -r p; do \
         printf '{tag}%s\\t%s\\t%s\\n' \"${{p#.}}\" \"$(stat -c%s \"$p\")\" \
         \"$(sha256sum \"$p\" | cut -d' ' -f1)\"; done)"
    )
}

/// `devid -> (size, used)` from `btrfs filesystem show --raw` lines the
/// script tagged `devid`.
fn shown_devices(text: &str) -> BTreeMap<u64, (u64, u64)> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            (f.len() == 4 && f[0] == "devid").then(|| {
                let n = |s: &str| s.parse::<u64>().unwrap_or_else(|_| panic!("{line}"));
                (n(f[1]), (n(f[2]), n(f[3])))
            })
        })
        .collect()
}

/// The value of the line the script tagged `tag`.
fn tagged<'a>(text: &'a str, tag: &str) -> &'a str {
    text.lines()
        .find_map(|l| l.strip_prefix(&format!("{tag}\t")))
        .unwrap_or_else(|| panic!("the script printed no {tag}:\n{text}"))
}

/// What the guest is asked about the pool's devices.
const SHOW_DEVICES: &str = "btrfs filesystem show --raw \"$MNT\" | \
     awk '$1 == \"devid\" {print \"devid\\t\" $2 \"\\t\" $4 \"\\t\" $6}'\n\
     btrfs filesystem show \"$MNT\" | awk '/Total devices/ {print \"total\\t\" $3}'\n\
     btrfs filesystem usage -b \"$MNT\" | awk '/Device size:/ {print \"size\\t\" $3}'\n";

/// Paths as the guest helpers take them.
fn images(paths: &[&PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

/// The kernel reads what is left after a removal: check clean, the
/// devices `want` (`devid -> size`) and nothing else, and every file of
/// `files`. Returns what it printed.
fn the_kernel_reads(
    left: &[&PathBuf],
    want: &BTreeMap<u64, u64>,
    files: &BTreeMap<String, (String, String)>,
) -> String {
    let left = images(left);
    let left: Vec<&str> = left.iter().map(String::as_str).collect();
    let read: PoolRead =
        guest_kernel_pool_read(&left, &format!("{SHOW_DEVICES}{}", list_files("file\\t")));
    assert_eq!(
        read.check_status, 0,
        "btrfs check --readonly does not find what the removal left clean:\n{}",
        read.check
    );
    let out = String::from_utf8_lossy(&read.out.stdout).into_owned();
    assert!(
        read.out.status.success(),
        "the kernel could not mount and read what the removal left:\n{out}{}",
        String::from_utf8_lossy(&read.out.stderr)
    );
    assert_eq!(tagged(&out, "total"), want.len().to_string(), "{out}");
    assert_eq!(
        tagged(&out, "size"),
        want.values().sum::<u64>().to_string(),
        "{out}"
    );
    let shown: BTreeMap<u64, u64> = shown_devices(&out)
        .into_iter()
        .map(|(id, (size, _))| (id, size))
        .collect();
    assert_eq!(
        &shown, want,
        "the kernel's devices after the removal:\n{out}"
    );
    assert_eq!(
        &manifest(&out, Some("file")),
        files,
        "the kernel reads the files differently from how it wrote them"
    );
    out
}

/// Every member's superblock counts `want`, the devices left, and the
/// removed device carries none at all.
fn the_superblocks_count(
    left: &[&PathBuf],
    removed: &Path,
    want: &BTreeMap<u64, u64>,
    generation: u64,
) {
    for path in left {
        let dev = FileDevice::open(path).expect("open a member");
        let (sb, copy) = fs_btrfs::superblock::read_superblock(&dev).expect("its superblock");
        let at = path.display();
        assert_eq!(copy, 0, "{at}: the primary is not the newest");
        assert_eq!(sb.generation, generation, "{at} missed a commit");
        assert_eq!(sb.num_devices, want.len() as u64, "{at}'s device count");
        assert_eq!(sb.total_bytes, want.values().sum::<u64>(), "{at}'s size");
        assert_eq!(
            want.get(&sb.dev_item.devid),
            Some(&sb.dev_item.total_bytes),
            "{at}'s own device item"
        );
    }
    assert!(
        fs_btrfs::superblock::read_superblock(&FileDevice::open(removed).unwrap()).is_err(),
        "{} still carries a superblock after its removal",
        removed.display()
    );
}

#[test]
fn devices_removed_leave_a_pool_and_then_a_volume_the_kernel_mounts_and_reads() {
    let scratch = Scratch::new("shrink");
    let [a, b, c] = [
        scratch.formatted("a.img", SIZES[0]),
        scratch.blank("b.img", SIZES[1]),
        scratch.blank("c.img", SIZES[2]),
    ];

    // The kernel writes the files, and the manifest is taken while it has
    // them mounted: nothing here has touched the volume yet.
    let written = guest_kernel_write_ok(
        &a.to_string_lossy(),
        "the kernel writes the files the removals must keep",
        &format!(
            "head -c 100000 /dev/urandom > \"$MNT/before.bin\"\n\
             mkdir \"$MNT/dir\"\n\
             head -c 5000 /dev/urandom > \"$MNT/dir/small.bin\"\n\
             printf hello > \"$MNT/dir/hello.txt\"\n\
             sync\n{}",
            list_files("")
        ),
    );
    let files = manifest(&written, None);
    assert_eq!(
        files.len(),
        3,
        "the kernel's manifest is not the three files written:\n{written}"
    );

    // Two devices added, then the first of them removed: devids 1 and 3.
    let mut fs = Filesystem::mount_rw(open_rw(&a)).expect("the volume mounts read-write");
    let generation = fs.superblock().generation;
    assert_eq!(fs.add_device(open_rw(&b)).expect("adding b"), 2);
    assert_eq!(fs.add_device(open_rw(&c)).expect("adding c"), 3);
    fs.remove_device(2)
        .unwrap_or_else(|e| panic!("removing device 2: {e}"));
    assert_eq!(
        fs.superblock().generation,
        generation + 3,
        "each add and each removal is one committed transaction"
    );
    let want = BTreeMap::from([(1, SIZES[0]), (3, SIZES[2])]);
    assert_eq!(fs.superblock().num_devices, 2);
    assert_eq!(fs.superblock().total_bytes, SIZES[0] + SIZES[2]);
    for (path, (size, digest)) in &files {
        let data = fs
            .read_path(path)
            .expect("a manifest file, through this driver");
        assert_eq!(
            (data.len().to_string(), sha256_hex(&data)),
            (size.clone(), digest.clone()),
            "{path} reads back differently through this driver after the removal"
        );
    }
    drop(fs);
    the_superblocks_count(&[&a, &c], &b, &want, generation + 3);
    the_kernel_reads(&[&a, &c], &want, &files);

    // The kernel fills the two devices left past what the first holds.
    let left = images(&[&a, &c]);
    let left: Vec<&str> = left.iter().map(String::as_str).collect();
    let filled = guest_kernel_pool_write(
        &left,
        &format!(
            "dd if=/dev/zero of=\"$MNT/big\" bs=1M count=300 status=none\nsync\n{SHOW_DEVICES}"
        ),
    );
    let out = String::from_utf8_lossy(&filled.out.stdout).into_owned();
    assert!(
        filled.out.status.success(),
        "the kernel could not fill the pool the removal left:\n{out}{}",
        String::from_utf8_lossy(&filled.out.stderr)
    );
    assert_eq!(
        filled.check_status, 0,
        "btrfs check --readonly does not find the pool clean after the kernel filled it:\n{}",
        filled.check
    );
    assert!(
        shown_devices(&out).get(&3).is_some_and(|d| d.1 > 0),
        "the kernel stored 300 MiB without allocating on device 3:\n{out}"
    );

    // The second device removed too: the volume is one device again.
    let mut fs = Filesystem::mount_pool_rw(vec![open_rw(&a), open_rw(&c)])
        .expect("the two devices left mount read-write");
    fs.remove_device(3)
        .unwrap_or_else(|e| panic!("removing device 3: {e}"));
    assert_eq!(fs.superblock().num_devices, 1);
    drop(fs);
    let want = BTreeMap::from([(1, SIZES[0])]);
    the_superblocks_count(&[&a], &c, &want, generation + 4);
    the_kernel_reads(&[&a], &want, &files);
}

/// Every refusal leaves every image exactly as it was.
#[test]
fn a_removal_that_cannot_be_made_is_refused_and_writes_nothing() {
    let scratch = Scratch::new("refused");
    let pool = [
        scratch.copy("btrfs-pool-a.img"),
        scratch.copy("btrfs-pool-b.img"),
    ];
    let volume = scratch.blank("volume.img", 256 * MIB);
    fs_btrfs::mkfs::format(&*open_rw(&volume), &fs_btrfs::mkfs::Options::default())
        .expect("this crate's mkfs");
    let all = [&pool[0], &pool[1], &volume];
    let before: Vec<Vec<u8>> = all.iter().map(|p| std::fs::read(p).unwrap()).collect();
    let unchanged = |what: &str| {
        for (path, bytes) in all.iter().zip(&before) {
            assert!(
                std::fs::read(path).unwrap() == *bytes,
                "{what}: {} was written to",
                path.display()
            );
        }
    };

    // A read-only mount.
    let readers: Vec<Arc<dyn BlockRead>> = pool
        .iter()
        .map(|p| Arc::new(FileDevice::open(p).expect("open")) as Arc<dyn BlockRead>)
        .collect();
    let mut ro = Filesystem::mount_pool(readers).expect("the pool mounts read-only");
    assert!(matches!(ro.remove_device(2), Err(Error::ReadOnly)));
    drop(ro);
    unchanged("a read-only mount");

    // A device holding chunks: every RAID1 chunk has a stripe on each.
    let mut fs = Filesystem::mount_pool_rw(pool.iter().map(|p| open_rw(p)).collect())
        .expect("the pool mounts read-write");
    let msg = fs
        .remove_device(2)
        .expect_err("a device holding chunks was removed")
        .to_string();
    assert!(msg.contains("balance"), "{msg}");

    // A devid that is not a member.
    let msg = fs
        .remove_device(9)
        .expect_err("device 9 was removed")
        .to_string();
    assert!(msg.contains("not a member"), "{msg}");
    drop(fs);
    unchanged("a device holding chunks, or not a member");

    // The only device.
    let mut fs = Filesystem::mount_rw(open_rw(&volume)).expect("mounts read-write");
    let msg = fs
        .remove_device(1)
        .expect_err("the only device was removed")
        .to_string();
    assert!(msg.contains("only device"), "{msg}");
    drop(fs);
    unchanged("the only device");
}
