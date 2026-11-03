//! The kernel oracle helper mounts a copied image after the kernel has
//! forgotten earlier scans of it (#738).
//!
//! A copy has its original's filesystem identifier, and the kernel keeps
//! a record of every identifier it has scanned, after the unmount too. A
//! scan of that identifier and device number at a LOWER generation through
//! a DIFFERENT device path is refused with `File exists`. The helper copies
//! an image and mounts the copy through whichever loop device is free, so
//! a record left by an earlier mount of a copy is a mount failure that has
//! nothing to do with the bytes.
//!
//! # How the record is forced
//!
//! The script below mounts a copy of the volume read-write, which advances
//! its generation, then scans its loop device after the unmount and keeps
//! it attached. That leaves the record a late udev scan leaves, and takes
//! the loop device's path out of `losetup --find`'s answer.
//!
//! The script then proves the record is in force: an older copy through a
//! second loop device has to be refused, so this test cannot pass because
//! the kernel stopped refusing.
//!
//! The script runs under a mount of a second volume, because the helpers
//! are the only way into the guest. That volume has another identifier,
//! so its own mount does not touch the record.
//!
//! This holds the forget. A scan racing the forget cannot be forced here;
//! `tests/scripts/kernel-helper-forget.sh` holds the order of the settle
//! and the forget.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use fs_btrfs::mkfs;
use fs_btrfs_test_support::{guest_kernel_read, guest_kernel_read_ok, guest_kernel_write_ok};
use fs_btrfs_test_support::{guest_quote, temp_path};
use fs_core::FileDevice;

/// Under `$VOLUME`, the image to copy. Prints the scratch directory and
/// the loop device the caller has to release.
const LEAVE_A_RECORD: &str = r#"
work="$(mktemp -d /var/tmp/fs-btrfs-record.XXXXXX)"
mkdir "$work/mnt"
cp --sparse=always "$VOLUME" "$work/newer"
cp --sparse=always "$VOLUME" "$work/older"
udevadm settle
btrfs device scan --forget >/dev/null
held="$(losetup --find --show "$work/newer")"
printf '%s\t%s\n' "$work" "$held"
mount -t btrfs "$held" "$work/mnt"
touch "$work/mnt/advances-the-generation"
umount "$work/mnt"
btrfs device scan "$held" >/dev/null
second="$(losetup --find --show --read-only "$work/older")"
status=0
mount -t btrfs -o ro "$second" "$work/mnt" 2> "$work/refusal" || status=$?
if [ "$status" = 0 ]; then
    umount "$work/mnt"
fi
losetup -d "$second"
if [ "$status" = 0 ]; then
    echo "an older copy mounted with a newer generation on record" >&2
    exit 1
fi
if ! grep -q 'File exists' "$work/refusal"; then
    cat "$work/refusal" >&2
    exit 1
fi
"#;

/// A record in the guest's kernel, and the loop device holding its path.
struct Record<'a> {
    carrier: &'a str,
    work: String,
    held: String,
}

impl<'a> Record<'a> {
    fn leave(carrier: &'a str, volume: &str) -> Self {
        let script = format!("VOLUME={}\n{LEAVE_A_RECORD}", guest_quote(volume));
        let out = guest_kernel_read_ok(carrier, "leave a record of a newer generation", &script);
        let (work, held) = out
            .trim_end()
            .split_once('\t')
            .unwrap_or_else(|| panic!("the record script printed {out:?}"));
        Record {
            carrier,
            work: work.to_string(),
            held: held.to_string(),
        }
    }
}

impl Drop for Record<'_> {
    /// Runs on a panic too: a record left behind would fail a later test
    /// in this tier, and the panic that matters is the one in flight.
    fn drop(&mut self) {
        let script = format!(
            "losetup -d {held}\nbtrfs device scan --forget >/dev/null\nrm -r {work}",
            held = guest_quote(&self.held),
            work = guest_quote(&self.work),
        );
        let _ = catch_unwind(AssertUnwindSafe(|| {
            guest_kernel_read(self.carrier, &script)
        }));
    }
}

/// A freshly formatted volume, with an identifier of its own.
fn format(dir: &Path, name: &str) -> String {
    let image: PathBuf = dir.join(name);
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(256 << 20))
        .expect("create the image");
    let dev = FileDevice::open_rw(&image).expect("open rw");
    mkfs::format(&dev, &mkfs::Options::default()).expect("format");
    image.to_string_lossy().into_owned()
}

#[test]
fn a_copy_mounts_with_a_newer_generation_on_record() {
    let dir = PathBuf::from(temp_path!("oracle-forgets-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    let carrier = format(&dir, "carrier.img");
    let volume = format(&dir, "volume.img");

    {
        let _record = Record::leave(&carrier, &volume);
        let listed = guest_kernel_read_ok(&volume, "a read-only mount of a copy", "ls -a \"$MNT\"");
        assert!(
            !listed.contains("advances-the-generation"),
            "the read-only mount read the newer copy, not the image it was given: {listed}"
        );
    }
    {
        let _record = Record::leave(&carrier, &volume);
        guest_kernel_write_ok(
            &volume,
            "a read-write mount of a copy",
            "touch \"$MNT/written\"",
        );
    }
    let listed = guest_kernel_read_ok(&volume, "the copy the write brought back", "ls \"$MNT\"");
    assert!(
        listed.contains("written"),
        "the read-write mount's file did not come back: {listed}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
