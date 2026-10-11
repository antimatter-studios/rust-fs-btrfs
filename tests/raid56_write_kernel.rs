//! RAID5 and RAID6 pools written through `Filesystem::mount_pool_rw` are
//! ones `btrfs check` finds clean and the kernel reads back, with parity
//! that agrees with the data (#299).
//!
//! # What a parity write has to get right
//!
//! A parity chunk holds one copy of each byte and, per full stripe, P
//! (and on RAID6 Q) computed from the data elements beside it. Writing a
//! data element without recomputing them leaves parity that rebuilds the
//! wrong bytes the day a device is lost, and nothing reads parity while
//! the data verifies, so nothing would notice until then. Every write --
//! a file's extents and the commit's tree blocks alike -- is therefore a
//! read-modify-write of each full stripe it touches.
//!
//! # The oracles
//!
//! The pools are `btrfs-raid5-nosum-*` and `btrfs-raid6-nosum-*`, made
//! and filled by the kernel in the harness VM, data and metadata both in
//! parity chunks; `nosum.bin` was written under `nodatasum`, which is
//! what the copy-on-write path takes. After this crate writes it, every
//! member is attached in the guest, `btrfs check --readonly` inspects the
//! pool, and the kernel mounts it and hashes every file: the written file
//! must read back as written, every other one as the manifest says the
//! kernel wrote it. Then this crate's scrub, which recomputes P and Q for
//! every full stripe holding data (#301), must find nothing wrong -- the
//! kernel's read-only scrub does not check parity.

use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{fixture, guest_kernel_pool_read, sha256_hex, temp_path};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// The pools' nodatasum file.
const NOSUM: &str = "/nosum.bin";

/// Copies of a pool's members, removed when dropped -- including on a
/// panic.
struct Scratch(PathBuf, usize);

impl Scratch {
    fn new(profile: &str, count: usize) -> Self {
        // In the repository's scratch directory: the harness VM sees this
        // repository and nothing else of the host.
        let dir = PathBuf::from(temp_path!(
            "btrfs-{profile}-write-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for i in 1..=count {
            let src = fixture(&format!("btrfs-{profile}-nosum-{i}.img"));
            std::fs::copy(&src, dir.join(format!("{i}.img")))
                .unwrap_or_else(|e| panic!("copying {}: {e}", src.display()));
        }
        Self(dir, count)
    }

    fn members(&self) -> Vec<PathBuf> {
        (1..=self.1)
            .map(|i| self.0.join(format!("{i}.img")))
            .collect()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn writable(members: &[PathBuf]) -> Vec<Arc<dyn BlockDevice>> {
    members
        .iter()
        .map(|p| {
            Arc::new(FileDevice::open_rw(p).expect("opening a pool member read-write"))
                as Arc<dyn BlockDevice>
        })
        .collect()
}

fn readable(members: &[PathBuf]) -> Vec<Arc<dyn BlockRead>> {
    members
        .iter()
        .map(|p| {
            Arc::new(FileDevice::open(p).expect("opening a pool member")) as Arc<dyn BlockRead>
        })
        .collect()
}

/// Every file the fixture's manifest names, `path -> (size, sha256)`.
fn manifest(profile: &str) -> BTreeMap<String, (String, String)> {
    let path = fixture(&format!("btrfs-{profile}-nosum.manifest"));
    let text = std::fs::read_to_string(path).expect("parity pool manifest");
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            (f.len() == 3 && f[1] != "dir")
                .then(|| (f[0].to_string(), (f[1].to_string(), f[2].to_string())))
        })
        .collect()
}

/// Several copy-on-write transactions on one pool mount, then every
/// oracle over the result.
fn a_written_pool_is_clean_and_reads_back(profile: &str, count: usize) {
    let scratch = Scratch::new(profile, count);
    let members = scratch.members();
    let mut want = manifest(profile);
    assert!(
        want.contains_key(NOSUM),
        "the {profile} pool fixture has no {NOSUM}; rebuild it with `chore fixtures`"
    );

    let mut fs = match Filesystem::mount_pool_rw(writable(&members)) {
        Ok(fs) => fs,
        Err(e) => panic!("a {profile} pool given every member should mount read-write: {e}"),
    };
    let ino = fs.lookup_path(NOSUM).expect("the nodatasum file").ino;
    let mut expected = fs.read_file(ino).expect("reading it before the write");
    assert!(
        expected.len() >= 300_000,
        "{NOSUM} is {} bytes",
        expected.len()
    );

    // Part of one element; across an element boundary and a full-stripe
    // boundary; and two whole full stripes from the start.
    let writes: [(u64, Vec<u8>); 3] = [
        (4096, b"written into one element ".repeat(40)),
        (100_000, vec![0x5a; 60_000]),
        (0, (0..262_144u32).map(|i| (i * 7 + 3) as u8).collect()),
    ];
    for (offset, bytes) in &writes {
        let n = fs.write(ino, *offset, bytes).unwrap_or_else(|e| {
            panic!("{profile}: writing {} bytes at {offset}: {e}", bytes.len())
        });
        assert_eq!(n, bytes.len(), "a short write");
        expected[*offset as usize..*offset as usize + bytes.len()].copy_from_slice(bytes);
        assert_eq!(
            fs.read_file(ino).expect("reading it back"),
            expected,
            "{profile}: the pool reads back differently through this driver after the write"
        );
    }
    drop(fs);

    // Parity that agrees with the data, on every full stripe holding any.
    let fs = Filesystem::mount_pool(readable(&members)).expect("the written pool mounts");
    let scrub = fs.scrub().expect("scrub");
    assert!(
        scrub.parity_stripes > 0,
        "{profile}: no full stripe was checked"
    );
    assert!(
        scrub.errors.is_empty(),
        "{profile}: the written pool's parity or copies are wrong: {:#?}",
        scrub.errors
    );
    drop(fs);

    want.insert(
        NOSUM.to_string(),
        (expected.len().to_string(), sha256_hex(&expected)),
    );
    let images: Vec<String> = members
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let images: Vec<&str> = images.iter().map(String::as_str).collect();
    let read = guest_kernel_pool_read(
        &images,
        "cd \"$MNT\"; find . -type f | sort | while read -r p; do \
         printf '%s\\t%s\\t%s\\n' \"${p#.}\" \"$(stat -c%s \"$p\")\" \
         \"$(sha256sum \"$p\" | cut -d' ' -f1)\"; done",
    );
    assert_eq!(
        read.check_status, 0,
        "{profile}: btrfs check --readonly does not find the written pool clean:\n{}",
        read.check
    );
    assert!(
        read.out.status.success(),
        "{profile}: the kernel could not mount and read the written pool:\n{}{}",
        String::from_utf8_lossy(&read.out.stdout),
        String::from_utf8_lossy(&read.out.stderr)
    );
    let kernel: BTreeMap<String, (String, String)> = String::from_utf8_lossy(&read.out.stdout)
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            (f.len() == 3).then(|| (f[0].to_string(), (f[1].to_string(), f[2].to_string())))
        })
        .collect();
    assert_eq!(
        kernel, want,
        "{profile}: the kernel reads the pool back differently from what was written"
    );
}

#[test]
fn a_raid5_pool_written_is_clean_and_reads_back_under_the_kernel() {
    a_written_pool_is_clean_and_reads_back("raid5", 3);
}

#[test]
fn a_raid6_pool_written_is_clean_and_reads_back_under_the_kernel() {
    a_written_pool_is_clean_and_reads_back("raid6", 4);
}
