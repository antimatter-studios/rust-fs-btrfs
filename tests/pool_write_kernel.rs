//! A two-device RAID1 pool written through `Filesystem::mount_pool_rw`
//! is one `btrfs check` finds clean and the kernel reads back (#298).
//!
//! # What a pool write has to get right
//!
//! A chunk stripe names the device it lives on. Every copy of a mirrored
//! block has to land on the disk its stripe names, at the offset it
//! names there — the same offset on the other disk is some other block
//! entirely — and a commit has to write every member's superblocks, each
//! keeping its own `dev_item`, or the members disagree about which
//! generation the pool is at and the kernel refuses to assemble it.
//!
//! # The oracles
//!
//! The pool is `btrfs-pool-{a,b}.img`, made and filled by the kernel in
//! the harness VM; `nosum.bin` was written under `nodatasum`, which is
//! what the copy-on-write path takes so far. After this crate writes it,
//! both members are attached in the guest, `btrfs check --readonly`
//! inspects the whole pool, and the kernel mounts it and hashes every
//! file. The written file must read back as written, and every other file
//! as the fixture's manifest says the kernel wrote it.

use fs_btrfs::fs::Filesystem;
use fs_btrfs_test_support::{fixture, guest_kernel_pool_read, sha256_hex, temp_path};
use fs_core::{BlockDevice, FileDevice};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// The pool's nodatasum file.
const NOSUM: &str = "/nosum.bin";

/// Copies of the pool's two members, removed when dropped — including
/// on a panic.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        // In the repository's scratch directory: the harness VM sees this
        // repository and nothing else of the host.
        let dir = PathBuf::from(temp_path!(
            "btrfs-pool-write-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for member in ["a", "b"] {
            let src = fixture(&format!("btrfs-pool-{member}.img"));
            std::fs::copy(&src, dir.join(format!("{member}.img")))
                .unwrap_or_else(|e| panic!("copying {}: {e}", src.display()));
        }
        Self(dir)
    }

    fn members(&self) -> Vec<PathBuf> {
        vec![self.0.join("a.img"), self.0.join("b.img")]
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn devices(members: &[PathBuf]) -> Vec<Arc<dyn BlockDevice>> {
    members
        .iter()
        .map(|p| {
            Arc::new(FileDevice::open_rw(p).expect("opening a pool member read-write"))
                as Arc<dyn BlockDevice>
        })
        .collect()
}

/// Every file the fixture's manifest names, `path -> (size, sha256)`.
fn manifest() -> BTreeMap<String, (String, String)> {
    let text = std::fs::read_to_string(fixture("btrfs-pool.manifest")).expect("pool manifest");
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            (f.len() == 3 && f[1] != "dir")
                .then(|| (f[0].to_string(), (f[1].to_string(), f[2].to_string())))
        })
        .collect()
}

/// Several copy-on-write transactions on one pool mount, then both
/// oracles over the result.
#[test]
fn a_pool_written_through_every_member_is_clean_and_reads_back_under_the_kernel() {
    let scratch = Scratch::new("cow");
    let members = scratch.members();
    let mut want = manifest();
    assert!(
        want.contains_key(NOSUM),
        "the pool fixture has no {NOSUM}; rebuild it with `chore fixtures`"
    );

    let mut fs = match Filesystem::mount_pool_rw(devices(&members)) {
        Ok(fs) => fs,
        Err(e) => panic!("a RAID1 pool given every member should mount read-write: {e}"),
    };
    assert!(fs.is_writable());
    let ino = fs.lookup_path(NOSUM).expect("the nodatasum file").ino;
    let mut expected = fs.read_file(ino).expect("reading it before the write");

    let generation = fs.superblock().generation;
    let writes: [(u64, Vec<u8>); 2] = [
        (4096, b"written to both members of a pool ".repeat(40)),
        (65_000, vec![0x5a; 9000]),
    ];
    for (offset, bytes) in &writes {
        let n = fs
            .write(ino, *offset, bytes)
            .unwrap_or_else(|e| panic!("writing {} bytes at {offset}: {e}", bytes.len()));
        assert_eq!(n, bytes.len(), "a short write");
        expected[*offset as usize..*offset as usize + bytes.len()].copy_from_slice(bytes);
        assert_eq!(
            fs.read_file(ino).expect("reading it back"),
            expected,
            "the pool reads back differently through this driver after the write"
        );
    }
    assert_eq!(
        fs.superblock().generation,
        generation + writes.len() as u64,
        "each write is one committed transaction"
    );
    drop(fs);

    // Every member's superblock moved, each still naming itself.
    for (index, member) in members.iter().enumerate() {
        let dev = FileDevice::open(member).expect("open a member");
        let (sb, copy) = fs_btrfs::superblock::read_superblock(&dev).expect("its superblock");
        assert_eq!(
            copy, 0,
            "member {index}: the primary superblock is the newest"
        );
        assert_eq!(
            sb.generation,
            generation + writes.len() as u64,
            "member {index} was not committed to"
        );
        assert_eq!(
            sb.dev_item.devid,
            index as u64 + 1,
            "member {index}'s superblock names another device"
        );
    }

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
        "btrfs check --readonly does not find the written pool clean:\n{}",
        read.check
    );
    assert!(
        read.out.status.success(),
        "the kernel could not mount and read the written pool:\n{}{}",
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
        "the kernel reads the pool back differently from what was written"
    );
}

/// A pool with a member left out is refused for writing, as for reading.
#[test]
fn a_pool_missing_a_member_is_refused_for_writing() {
    let scratch = Scratch::new("missing");
    let members = scratch.members();
    let msg = match Filesystem::mount_pool_rw(devices(&members[..1])) {
        Ok(_) => panic!("one member of a two-device pool was mounted read-write"),
        Err(e) => e.to_string(),
    };
    assert!(msg.contains("spans 2 devices"), "{msg}");
}

/// A member opened read-only is refused before anything is read.
#[test]
fn a_read_only_member_is_refused() {
    let scratch = Scratch::new("readonly");
    let members = scratch.members();
    let mut devs = devices(&members[..1]);
    devs.push(Arc::new(
        FileDevice::open(&members[1]).expect("open read-only"),
    ));
    assert!(matches!(
        Filesystem::mount_pool_rw(devs),
        Err(fs_btrfs::error::Error::ReadOnly)
    ));
}
