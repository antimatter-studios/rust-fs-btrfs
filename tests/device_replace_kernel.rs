//! Devices replaced through `Filesystem::replace_device`, judged by what is
//! not this crate (#303).
//!
//! # What a replace has to get right
//!
//! The new device takes the old one's place whole: every byte of every
//! device extent at the offset it had, and the old device's superblock, so
//! that it is the same devid with the same device UUID and the same
//! recorded size. Every chunk stripe names its device by devid and UUID
//! and every device extent by its offset, so a byte copied to the wrong
//! offset is a file the kernel reads wrong, and a device item that is not
//! the old one's is a stripe the kernel cannot find. The old device's
//! superblocks are wiped, or a scan finds two devices claiming one place.
//!
//! # The oracles
//!
//! The kernel makes every filesystem and writes its files, and the
//! manifest of them is taken then, before anything here touches them: a
//! RAID1 pool and a RAID5 pool (the fixtures), and a single-device volume.
//! This crate replaces one member of each with a larger image. In the
//! harness VM, with the new device attached and the old one not: `btrfs
//! check --readonly` over the result, `btrfs filesystem show` for each
//! devid's size and what is allocated on it, `dump-super` on the new
//! device, and the kernel reading every file of the manifest. Then the
//! kernel mounts the result read-write and writes to it, and `btrfs check`
//! looks again.

use fs_btrfs::error::Error;
use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{
    dump_super, fixture, guest_kernel_pool_read, guest_kernel_pool_write, guest_kernel_write_ok,
    oracle, sha256_hex, temp_path,
};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MIB: u64 = 1 << 20;

/// How much larger each new device is than the one it replaces.
const GROWTH: u64 = 64 * MIB;

/// A scratch directory of images, removed when it drops — including on a
/// panic.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        // In the repository's scratch directory: the harness VM sees this
        // repository and nothing else of the host.
        let dir = PathBuf::from(temp_path!(
            "btrfs-device-replace-{name}-{}-{:?}",
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
/// starting with `tag\t` when a tag is given; a fixture manifest's
/// directories, sized `dir`, are left out.
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
            (f.len() == 3 && f[1] != "dir")
                .then(|| (f[0].to_string(), (f[1].to_string(), f[2].to_string())))
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
     btrfs filesystem show \"$MNT\" | awk '/Total devices/ {print \"total\\t\" $3}'\n";

/// What a member's own superblock says it is: its devid, device UUID,
/// recorded size and bytes allocated.
fn member(path: &Path) -> (u64, [u8; 16], u64, u64) {
    let dev = FileDevice::open(path).expect("open a member");
    let (sb, copy) = fs_btrfs::superblock::read_superblock(&dev).expect("its superblock");
    assert_eq!(copy, 0, "{}: the primary is not the newest", path.display());
    let d = sb.dev_item;
    (d.devid, d.uuid, d.total_bytes, d.bytes_used)
}

/// The value `dump-super` gives `name`.
fn dumped(text: &str, name: &str) -> String {
    text.lines()
        .find_map(|l| {
            let mut words = l.split_whitespace();
            (words.next() == Some(name)).then(|| words.next().unwrap_or("").to_string())
        })
        .unwrap_or_else(|| panic!("dump-super has no {name}:\n{text}"))
}

/// The same 16 bytes as `dump-super` prints a UUID.
fn uuid_text(u: &[u8; 16]) -> String {
    let h: String = u.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// Replace the member at `members[index]`, which is device `devid`, with a
/// new image larger than it, and hold the result to every oracle with the
/// new image attached in its place and the old one not.
fn a_replaced_device_leaves_a_filesystem_the_kernel_mounts_without_it(
    scratch: &Scratch,
    mut members: Vec<PathBuf>,
    index: usize,
    want: BTreeMap<String, (String, String)>,
) {
    let before: Vec<_> = members.iter().map(|m| member(m)).collect();
    let (devid, uuid, recorded, used) = before[index];
    let old = members[index].clone();
    let old_len = std::fs::metadata(&old).unwrap().len();
    let new = scratch.blank("replacement.img", old_len + GROWTH);

    let devices: Vec<Arc<dyn BlockDevice>> = members.iter().map(|m| open_rw(m)).collect();
    let mut fs = Filesystem::mount_pool_rw(devices).expect("the filesystem mounts read-write");
    let sb = fs.superblock().clone();
    fs.replace_device(devid, open_rw(&new))
        .unwrap_or_else(|e| panic!("replacing device {devid}: {e}"));
    assert_eq!(
        (
            fs.superblock().generation,
            fs.superblock().num_devices,
            fs.superblock().total_bytes
        ),
        (sb.generation, sb.num_devices, sb.total_bytes),
        "a replace changes nothing the superblock says about the filesystem"
    );
    // The handle is the one with the new device in the old one's place.
    for (path, (size, digest)) in &want {
        let data = fs
            .read_path(path)
            .expect("a manifest file, through this driver");
        assert_eq!(
            (data.len().to_string(), sha256_hex(&data)),
            (size.clone(), digest.clone()),
            "{path} reads back differently through this driver after the replace"
        );
    }
    drop(fs);

    // The old device is no longer a member; the new one is it, whole.
    assert!(
        fs_btrfs::superblock::read_superblock(&FileDevice::open(&old).unwrap()).is_err(),
        "the replaced device still carries a superblock"
    );
    members[index] = new.clone();
    for (index, path) in members.iter().enumerate() {
        assert_eq!(
            member(path),
            before[index],
            "member {index} is not what the device it stands for was"
        );
    }
    let text = dump_super(&new);
    assert_eq!(dumped(&text, "dev_item.devid"), devid.to_string(), "{text}");
    assert_eq!(dumped(&text, "dev_item.uuid"), uuid_text(&uuid), "{text}");
    assert_eq!(
        dumped(&text, "dev_item.total_bytes"),
        recorded.to_string(),
        "{text}"
    );
    assert_eq!(
        dumped(&text, "dev_item.bytes_used"),
        used.to_string(),
        "{text}"
    );

    // The kernel: check, show, and every file of the manifest.
    let images: Vec<String> = members
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let images: Vec<&str> = images.iter().map(String::as_str).collect();
    let read = guest_kernel_pool_read(&images, &format!("{SHOW_DEVICES}{}", list_files("file\\t")));
    assert_eq!(
        read.check_status, 0,
        "btrfs check --readonly does not find the result of the replace clean:\n{}",
        read.check
    );
    let out = String::from_utf8_lossy(&read.out.stdout).into_owned();
    assert!(
        read.out.status.success(),
        "the kernel could not mount and read the result of the replace:\n{out}{}",
        String::from_utf8_lossy(&read.out.stderr)
    );
    assert_eq!(tagged(&out, "total"), members.len().to_string(), "{out}");
    let shown = shown_devices(&out);
    let expected: BTreeMap<u64, (u64, u64)> = before
        .iter()
        .map(|&(id, _, size, used)| (id, (size, used)))
        .collect();
    assert_eq!(
        shown, expected,
        "the kernel's devices are not the ones before the replace:\n{out}"
    );
    assert_eq!(
        manifest(&out, Some("file")),
        want,
        "the kernel reads the files differently from how it wrote them"
    );

    // The kernel writes to the result, and check looks again.
    let written = guest_kernel_pool_write(
        &images,
        "dd if=/dev/urandom of=\"$MNT/after-replace\" bs=1M count=16 status=none\nsync\n",
    );
    assert!(
        written.out.status.success(),
        "the kernel could not write to the result of the replace:\n{}",
        String::from_utf8_lossy(&written.out.stderr)
    );
    assert_eq!(
        written.check_status, 0,
        "btrfs check --readonly does not find the result clean after the kernel wrote to it:\n{}",
        written.check
    );
}

/// A RAID1 member, every chunk of the pool mirrored onto it.
#[test]
fn a_replaced_raid1_member_leaves_a_pool_the_kernel_mounts_without_it() {
    let scratch = Scratch::new("raid1");
    let members = vec![
        scratch.copy("btrfs-pool-a.img"),
        scratch.copy("btrfs-pool-b.img"),
    ];
    let text = std::fs::read_to_string(fixture("btrfs-pool.manifest")).expect("pool manifest");
    a_replaced_device_leaves_a_filesystem_the_kernel_mounts_without_it(
        &scratch,
        members,
        1,
        manifest(&text, None),
    );
}

/// A RAID5 member, and the lowest devid, which the mount reads the trees
/// through first: data and parity both on it.
#[test]
fn a_replaced_raid5_member_leaves_a_pool_the_kernel_mounts_without_it() {
    let scratch = Scratch::new("raid5");
    let members = (1..=3)
        .map(|i| scratch.copy(&format!("btrfs-raid5-nosum-{i}.img")))
        .collect();
    let text = std::fs::read_to_string(fixture("btrfs-raid5-nosum.manifest"))
        .expect("parity pool manifest");
    a_replaced_device_leaves_a_filesystem_the_kernel_mounts_without_it(
        &scratch,
        members,
        0,
        manifest(&text, None),
    );
}

/// A single-device volume's only device: the new one is the whole
/// filesystem afterwards.
#[test]
fn a_replaced_only_device_is_the_whole_volume_the_kernel_mounts() {
    let scratch = Scratch::new("single");
    let volume = scratch.formatted("a.img", 256 * MIB);
    let written = guest_kernel_write_ok(
        &volume.to_string_lossy(),
        "the kernel writes the files the replace must keep",
        &format!(
            "head -c 100000 /dev/urandom > \"$MNT/before.bin\"\n\
             mkdir \"$MNT/dir\"\n\
             head -c 5000 /dev/urandom > \"$MNT/dir/small.bin\"\n\
             printf hello > \"$MNT/dir/hello.txt\"\n\
             sync\n{}",
            list_files("")
        ),
    );
    let want = manifest(&written, None);
    assert_eq!(
        want.len(),
        3,
        "the kernel's manifest is not the three files written:\n{written}"
    );
    a_replaced_device_leaves_a_filesystem_the_kernel_mounts_without_it(
        &scratch,
        vec![volume],
        0,
        want,
    );
}

/// Every refusal leaves every image exactly as it was.
#[test]
fn a_replace_that_cannot_be_made_is_refused_and_writes_nothing() {
    let scratch = Scratch::new("refused");
    let volume = scratch.blank("a.img", 256 * MIB);
    fs_btrfs::mkfs::format(&*open_rw(&volume), &fs_btrfs::mkfs::Options::default())
        .expect("this crate's mkfs");
    let small = scratch.blank("small.img", 128 * MIB);
    let spare = scratch.blank("spare.img", 320 * MIB);
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
        ro.replace_device(1, open_rw(&spare)),
        Err(Error::ReadOnly)
    ));
    drop(ro);
    let mut fs = Filesystem::mount_rw(open_rw(&volume)).expect("mounts read-write");
    assert!(matches!(
        fs.replace_device(1, Arc::new(FileDevice::open(&spare).expect("open"))),
        Err(Error::ReadOnly)
    ));
    unchanged("a read-only mount or device");

    // A devid that is not a member.
    let msg = fs
        .replace_device(7, open_rw(&spare))
        .expect_err("device 7 was replaced")
        .to_string();
    assert!(msg.contains("not a member"), "{msg}");
    unchanged("a devid that is not a member");

    // A device smaller than the one it replaces.
    let msg = fs
        .replace_device(1, open_rw(&small))
        .expect_err("a smaller device took a larger one's place")
        .to_string();
    assert!(msg.contains("fewer than"), "{msg}");
    unchanged("a device too small");

    // A member, given as its own replacement.
    let msg = fs
        .replace_device(1, open_rw(&volume))
        .expect_err("a member replaced itself")
        .to_string();
    assert!(msg.contains("already carries a superblock"), "{msg}");
    unchanged("a member given as the replacement");
    assert_eq!(fs.superblock().num_devices, 1);
}
