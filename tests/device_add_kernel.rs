//! Devices added to a filesystem through `Filesystem::add_device`, judged
//! by what is not this crate (#303).
//!
//! # What an add has to get right
//!
//! A device joins in one committed transaction: a `DEV_ITEM` for it in
//! the chunk tree, `num_devices` and `total_bytes` raised in every
//! member's superblock, each member keeping its own `dev_item`, and a
//! superblock on the new device naming itself. Get the chunk tree and the
//! superblocks out of step and the kernel will not assemble the pool;
//! get a size wrong and `btrfs check` reports the superblock smaller or
//! larger than its devices.
//!
//! # The oracles
//!
//! The kernel makes the filesystem and writes its files, and a manifest
//! of them is taken then, before anything here touches it. This crate
//! adds a second device to the single-device volume, then a third to the
//! pool that made, and writes a directory on the result. In the harness
//! VM, with every member attached: `btrfs check --readonly` over the
//! pool, `btrfs filesystem show` and `btrfs filesystem usage` for what
//! the kernel believes the devices and their sizes are, `dump-super` on
//! the last device added, and the kernel reading every file of the
//! manifest. Then the kernel mounts the pool read-write and fills it past
//! what the first device holds, which it can only do by allocating on the
//! devices added, and `btrfs check` looks again.

use fs_btrfs::error::Error;
use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{
    dump_super, guest_kernel_pool_read, guest_kernel_pool_write, guest_kernel_write_ok, oracle,
    sha256_hex, temp_path,
};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MIB: u64 = 1 << 20;

/// The members' sizes: the volume the kernel makes, and the two added.
const SIZES: [u64; 3] = [256 * MIB, 256 * MIB, 128 * MIB];

/// A scratch directory of images, removed when it drops — including on a
/// panic.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        // In the repository's scratch directory: the harness VM sees this
        // repository and nothing else of the host.
        let dir = PathBuf::from(temp_path!(
            "btrfs-device-add-{name}-{}-{:?}",
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

#[test]
fn devices_added_one_at_a_time_make_a_pool_the_kernel_mounts_reads_and_fills() {
    let scratch = Scratch::new("grow");
    let members = [
        scratch.formatted("a.img", SIZES[0]),
        scratch.blank("b.img", SIZES[1]),
        scratch.blank("c.img", SIZES[2]),
    ];

    // The kernel writes the files, and the manifest is taken while it has
    // them mounted: nothing here has touched the volume yet.
    let written = guest_kernel_write_ok(
        &members[0].to_string_lossy(),
        "the kernel writes the files the add must keep",
        &format!(
            "head -c 100000 /dev/urandom > \"$MNT/before.bin\"\n\
             mkdir \"$MNT/dir\"\n\
             head -c 5000 /dev/urandom > \"$MNT/dir/small.bin\"\n\
             printf hello > \"$MNT/dir/hello.txt\"\n\
             sync\n{}",
            list_files("")
        ),
    );
    let mut want = manifest(&written, None);
    assert_eq!(
        want.len(),
        3,
        "the kernel's manifest is not the three files written:\n{written}"
    );

    // One device, then a pool of two: each add starts from the mount the
    // last one left.
    let mut fs = Filesystem::mount_rw(open_rw(&members[0])).expect("the volume mounts read-write");
    let generation = fs.superblock().generation;
    let first = fs.add_device(open_rw(&members[1])).expect("adding b");
    assert_eq!(first, 2, "the first device added is not device 2");
    let second = fs.add_device(open_rw(&members[2])).expect("adding c");
    assert_eq!(second, 3, "the second device added is not device 3");
    let total: u64 = SIZES.iter().sum();
    assert_eq!(fs.superblock().num_devices, 3);
    assert_eq!(fs.superblock().total_bytes, total);
    assert_eq!(
        fs.superblock().generation,
        generation + 2,
        "each add is one committed transaction"
    );

    // The handle is the pool's: it writes to every member and reads the
    // files the kernel wrote.
    let root = fs.lookup_path("/").expect("the root").ino;
    fs.mkdir(root, b"after-add", 0o755, 0, 0)
        .unwrap_or_else(|e| panic!("mkdir on the pool the adds made: {e}"));
    for (path, (size, digest)) in &want {
        let data = fs
            .read_path(path)
            .expect("a manifest file, through this driver");
        assert_eq!(
            (data.len().to_string(), sha256_hex(&data)),
            (size.clone(), digest.clone()),
            "{path} reads back differently through this driver after the adds"
        );
    }
    let generation = fs.superblock().generation;
    drop(fs);

    // Every member's superblock is the pool's, each naming itself.
    let mut uuids = Vec::new();
    for (index, member) in members.iter().enumerate() {
        let dev = FileDevice::open(member).expect("open a member");
        let (sb, copy) = fs_btrfs::superblock::read_superblock(&dev).expect("its superblock");
        assert_eq!(copy, 0, "member {index}: the primary is not the newest");
        assert_eq!(sb.generation, generation, "member {index} missed a commit");
        assert_eq!(sb.num_devices, 3, "member {index}'s device count");
        assert_eq!(sb.total_bytes, total, "member {index}'s size");
        assert_eq!(
            sb.dev_item.devid,
            index as u64 + 1,
            "member {index}'s devid"
        );
        assert_eq!(
            sb.dev_item.total_bytes, SIZES[index],
            "member {index}'s own size"
        );
        uuids.push(sb.dev_item.uuid);
    }
    uuids.sort();
    uuids.dedup();
    assert_eq!(uuids.len(), 3, "two members share a device uuid");

    // dump-super, on the device added last, reads it as a member.
    let text = dump_super(&members[2]);
    let field = |name: &str| {
        text.lines()
            .find_map(|l| {
                let mut words = l.split_whitespace();
                (words.next() == Some(name)).then(|| words.next().unwrap_or("").to_string())
            })
            .unwrap_or_else(|| panic!("dump-super has no {name}:\n{text}"))
    };
    assert_eq!(field("num_devices"), "3", "{text}");
    assert_eq!(field("total_bytes"), total.to_string(), "{text}");
    assert_eq!(field("dev_item.devid"), "3", "{text}");
    assert_eq!(
        field("dev_item.total_bytes"),
        SIZES[2].to_string(),
        "{text}"
    );
    assert_eq!(field("dev_item.bytes_used"), "0", "{text}");

    // The kernel: check, show, usage, and every file of the manifest.
    let images: Vec<String> = members
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let images: Vec<&str> = images.iter().map(String::as_str).collect();
    let read = guest_kernel_pool_read(
        &images,
        &format!(
            "{SHOW_DEVICES}test -d \"$MNT/after-add\" && printf 'after-add\\tdir\\n'\n{}",
            list_files("file\\t")
        ),
    );
    assert_eq!(
        read.check_status, 0,
        "btrfs check --readonly does not find the grown pool clean:\n{}",
        read.check
    );
    let out = String::from_utf8_lossy(&read.out.stdout).into_owned();
    assert!(
        read.out.status.success(),
        "the kernel could not mount and read the grown pool:\n{out}{}",
        String::from_utf8_lossy(&read.out.stderr)
    );
    assert_eq!(tagged(&out, "total"), "3", "{out}");
    assert_eq!(tagged(&out, "size"), total.to_string(), "{out}");
    assert_eq!(tagged(&out, "after-add"), "dir", "{out}");
    let shown = shown_devices(&out);
    assert_eq!(
        shown.keys().copied().collect::<Vec<_>>(),
        vec![1, 2, 3],
        "{out}"
    );
    for (devid, size) in (1u64..).zip(SIZES) {
        assert_eq!(shown[&devid].0, size, "device {devid}'s size:\n{out}");
    }
    assert_eq!(
        (shown[&2].1, shown[&3].1),
        (0, 0),
        "an added device has something allocated on it before the kernel wrote anything:\n{out}"
    );
    assert_eq!(
        manifest(&out, Some("file")),
        std::mem::take(&mut want),
        "the kernel reads the grown pool's files differently from what it wrote"
    );

    // The kernel fills the pool past what the first device holds: only
    // the devices added have room for the rest.
    let filled = guest_kernel_pool_write(
        &images,
        &format!(
            "dd if=/dev/zero of=\"$MNT/big\" bs=1M count=300 status=none\nsync\n{SHOW_DEVICES}"
        ),
    );
    let out = String::from_utf8_lossy(&filled.out.stdout).into_owned();
    assert!(
        filled.out.status.success(),
        "the kernel could not fill the grown pool:\n{out}{}",
        String::from_utf8_lossy(&filled.out.stderr)
    );
    assert_eq!(
        filled.check_status, 0,
        "btrfs check --readonly does not find the pool clean after the kernel filled it:\n{}",
        filled.check
    );
    let shown = shown_devices(&out);
    assert!(
        shown.get(&2).is_some_and(|d| d.1 > 0) || shown.get(&3).is_some_and(|d| d.1 > 0),
        "the kernel stored 300 MiB without allocating on a device added:\n{out}"
    );
}

/// Every refusal leaves every image exactly as it was.
#[test]
fn an_add_that_cannot_be_made_is_refused_and_writes_nothing() {
    let scratch = Scratch::new("refused");
    let volume = scratch.blank("a.img", 256 * MIB);
    fs_btrfs::mkfs::format(&*open_rw(&volume), &fs_btrfs::mkfs::Options::default())
        .expect("this crate's mkfs");
    let small = scratch.blank("small.img", MIB);
    let spare = scratch.blank("spare.img", 128 * MIB);
    let images = [&volume, &small, &spare];
    let before: Vec<Vec<u8>> = images.iter().map(|p| std::fs::read(p).unwrap()).collect();
    let unchanged = |what: &str| {
        for (path, bytes) in images.iter().zip(&before) {
            assert!(
                std::fs::read(path).unwrap() == *bytes,
                "{what}: {} was written to",
                path.display()
            );
        }
    };

    // A read-only mount, or a device that cannot be written.
    let mut ro =
        Filesystem::mount(Arc::new(FileDevice::open(&volume).expect("open")) as Arc<dyn BlockRead>)
            .expect("mounts read-only");
    assert!(matches!(
        ro.add_device(open_rw(&spare)),
        Err(Error::ReadOnly)
    ));
    drop(ro);
    let mut fs = Filesystem::mount_rw(open_rw(&volume)).expect("mounts read-write");
    assert!(matches!(
        fs.add_device(Arc::new(FileDevice::open(&spare).expect("open"))),
        Err(Error::ReadOnly)
    ));
    unchanged("a read-only mount or device");

    // No room past the first MiB.
    let msg = fs
        .add_device(open_rw(&small))
        .expect_err("a 1 MiB device was added")
        .to_string();
    assert!(msg.contains("no room on it for a chunk"), "{msg}");
    unchanged("a device too small");

    // A member, given again.
    let msg = fs
        .add_device(open_rw(&volume))
        .expect_err("a member was added to its own filesystem")
        .to_string();
    assert!(msg.contains("already carries a superblock"), "{msg}");
    unchanged("a member given again");
    assert_eq!(fs.superblock().num_devices, 1);
}
