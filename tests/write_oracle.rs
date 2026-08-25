//! An in-place write must leave a filesystem `btrfs check` still calls
//! valid, holding exactly the bytes we asked for.
//!
//! Reading our own write back through our own driver would prove only
//! self-consistency: the same misunderstanding of the chunk map would
//! place the write and then find it again. So the bytes are read back by
//! the Linux kernel through its own driver, and the checker then
//! inspects the whole volume — a write that landed correctly could still
//! have run past its extent into something the file's own contents would
//! never reveal.
//!
//! The write happens on the host, because the driver is Rust and the
//! oracle VM has no Rust toolchain; the verification happens in the VM,
//! where the tooling and a kernel are. `scripts/vm.sh` bridges them.
//!
//! Fixtures are gitignored and the VM is not always up, so this skips
//! rather than fails when either is missing. Generate the fixture with
//! `./scripts/vm-build-fixtures.sh`.

use fs_btrfs::Filesystem;
use fs_core::FileDevice;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// A `chattr +C` file: written in place, unchecksummed, unshared.
const INPLACE: &str = "/nc/inplace.bin";
/// An ordinary file on the same volume, which must be refused.
const COW: &str = "/cow.bin";

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

/// A working copy in the shared folder, removed when it drops —
/// including on a panic. Every other suite here treats each `.img` there
/// as a fixture to check, so one left behind fails unrelated tests.
struct Scratch(PathBuf);

impl Scratch {
    fn from(source: &Path, name: &str) -> Self {
        let path = share().join(name);
        std::fs::copy(source, &path).expect("copy the fixture");
        Scratch(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn fixture() -> Option<PathBuf> {
    let p = share().join("btrfs-nodatacow.img");
    p.exists().then_some(p)
}

fn vm_run(script: &str) -> Option<String> {
    let out = Command::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/vm.sh"))
        .arg("run")
        .arg(script)
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!(
            "vm.sh run failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn ino_of(fs: &Filesystem, path: &str) -> u64 {
    fs.lookup_path(path)
        .unwrap_or_else(|e| panic!("{path}: {e}"))
        .ino
}

/// The whole point: overwrite a `nodatacow` file and have Linux agree.
#[test]
fn an_in_place_write_survives_the_kernel_and_the_checker() {
    let Some(source) = fixture() else {
        eprintln!("no btrfs-nodatacow fixture — skipping");
        return;
    };
    let scratch = Scratch::from(&source, "btrfs-write.img");
    let img = scratch.path();

    let (offset, payload) = (8192u64, b"in-place, no copy-on-write\n".repeat(8));
    let expected = {
        let dev = FileDevice::open(img).expect("open read-only");
        let fs = Filesystem::mount(Arc::new(dev)).expect("mount read-only");
        let ino = ino_of(&fs, INPLACE);
        let mut whole = fs.read_file(ino).expect("read the file");
        assert!(
            offset as usize + payload.len() < whole.len(),
            "the payload must land inside the file, not extend it"
        );
        whole[offset as usize..offset as usize + payload.len()].copy_from_slice(&payload);
        sha256_hex(&whole)
    };

    {
        let dev = FileDevice::open_rw(img).expect("open read-write");
        let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
        let ino = ino_of(&fs, INPLACE);
        let n = fs
            .write_at(ino, offset, &payload)
            .expect("the write must be accepted");
        assert_eq!(n, payload.len(), "a short write should not be possible");
    }

    let script = format!(
        r#"
        set -e
        cp /share/btrfs-write.img /tmp/w.img
        echo "CHECK_BEGIN"
        btrfs check /tmp/w.img 2>&1 && echo "CHECK_RC=0" || echo "CHECK_RC=$?"
        echo "CHECK_END"
        mnt=$(mktemp -d)
        mount -o ro,loop /tmp/w.img "$mnt"
        echo "SHA $(sha256sum "$mnt{INPLACE}" | cut -d' ' -f1)"
        umount "$mnt"; rmdir "$mnt"; rm -f /tmp/w.img
        "#
    );
    let Some(out) = vm_run(&script) else {
        eprintln!("oracle VM unavailable — skipping verification");
        return;
    };

    let got = out
        .lines()
        .find_map(|l| l.strip_prefix("SHA "))
        .unwrap_or_else(|| panic!("the VM did not report a hash:\n{out}"))
        .trim();
    assert_eq!(
        got, expected,
        "the kernel reads back different bytes than were written\n{out}"
    );

    let report: String = out
        .lines()
        .skip_while(|l| !l.starts_with("CHECK_BEGIN"))
        .take_while(|l| !l.starts_with("CHECK_END"))
        .collect::<Vec<_>>()
        .join("\n");
    // The checker's exit status, not a keyword scan: its normal output
    // ends "no error found", which any search for "error" matches.
    assert!(
        report.contains("CHECK_RC=0"),
        "the checker rejected the filesystem after an in-place write:\n{report}"
    );
}

/// An ordinary copy-on-write file on the same volume must be refused.
///
/// Without this the test above would pass equally well on a driver that
/// wrote in place regardless of the flag — which is precisely the bug
/// worth guarding against, since such a driver would corrupt every
/// normal Btrfs file while looking correct on this fixture.
#[test]
fn a_copy_on_write_file_is_refused() {
    let Some(source) = fixture() else {
        eprintln!("no btrfs-nodatacow fixture — skipping");
        return;
    };
    let scratch = Scratch::from(&source, "btrfs-cow-refused.img");
    let img = scratch.path();

    let before = {
        let dev = FileDevice::open(img).expect("open");
        let fs = Filesystem::mount(Arc::new(dev)).expect("mount");
        sha256_hex(&fs.read_file(ino_of(&fs, COW)).expect("read"))
    };

    let dev = FileDevice::open_rw(img).expect("open read-write");
    let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
    let ino = ino_of(&fs, COW);
    let err = fs
        .write_at(ino, 0, b"this must not land")
        .expect_err("a copy-on-write file must be refused");
    assert!(
        format!("{err}").contains("copy-on-write"),
        "the refusal should name why: {err}"
    );

    let after = sha256_hex(&fs.read_file(ino).expect("read"));
    assert_eq!(before, after, "a refused write still changed the file");
}

/// A read-only mount refuses, and leaves the volume untouched.
#[test]
fn a_read_only_mount_refuses_to_write() {
    let Some(source) = fixture() else {
        eprintln!("no btrfs-nodatacow fixture — skipping");
        return;
    };
    let scratch = Scratch::from(&source, "btrfs-ro.img");
    let img = scratch.path();

    let dev = FileDevice::open(img).expect("open read-only");
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount read-only");
    let ino = ino_of(&fs, INPLACE);
    let err = fs
        .write_at(ino, 0, b"nope")
        .expect_err("a read-only mount must refuse");
    assert!(matches!(err, fs_btrfs::Error::ReadOnly), "got {err}");
}
