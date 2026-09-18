//! THE KERNEL ORACLE: the real in-kernel btrfs driver, reading back
//! what this crate wrote.
//!
//! btrfs-progs is a second opinion on the same file format, written by
//! the same project, from the same specification. Linux is the thing
//! the images are actually for. A volume `btrfs check` calls clean can
//! still be one the kernel mounts differently — a directory entry it
//! will not find, an extent it reads shorter than we wrote it, a
//! checksum it recomputes to something else — and nothing on the host
//! can catch that, because a host mount is the kernel's job and macOS
//! has no btrfs at all.
//!
//! So a kernel oracle is a script run INSIDE the harness VM against a
//! loop mount of one of our images. It is the only place in this suite
//! where a filesystem is mounted, and it never happens on the host —
//! and never on a CI runner either, which is what used to make these
//! checks impossible from a Mac.
//!
//! ONE GUEST CALL PER CHECK. The script does the whole comparison —
//! mount, walk, hash, read attributes, unmount — and prints what it
//! found, rather than paying a round trip per question.
//!
//! THE IMAGE IS COPIED INTO THE GUEST'S OWN DISK for the mount, and
//! copied back afterwards when the test asked for a writable mount. A
//! loop mount reads through the page cache and writes back at its own
//! pace; pointing that at a file on the 9p share mixes two caches over
//! one file, and the host would be reading bytes the guest has not
//! written yet. The copy is sparse, so even the 2 GiB populated
//! fixtures cost a fraction of a second.

use std::collections::BTreeMap;
use std::process::Output;
use std::sync::Mutex;

use crate::oracle::{guest_quote, guest_shell, repo, session, Run};

/// Mount `image` read-only in the guest and run `script` against it.
///
/// `$MNT` is the mount point, and `$IMG` the image, inside the guest.
/// The script runs under `bash -euo pipefail`, so an unchecked failure
/// inside it fails the call. Its stdout, its stderr and its exit status
/// come back exactly as they were.
///
/// The mount is `ro` AND the loop device is read-only, so a mount that
/// would have replayed a log or bumped the generation cannot change the
/// image behind the test's back — which matters more here than on any
/// other filesystem in the family, because a btrfs fixture is compared
/// against a superblock dump taken straight out of `mkfs.btrfs`, and a
/// single read-write mount moves `generation` and invalidates it.
#[track_caller]
pub fn guest_kernel_read(image: &str, script: &str) -> Output {
    run(image, script, Mount::ReadOnly)
}

/// Mount `image` read-write in the guest, run `script`, and bring the
/// image back to the host with whatever the kernel wrote in it.
///
/// The reverse direction: the kernel writes, this crate reads.
#[track_caller]
pub fn guest_kernel_write(image: &str, script: &str) -> Output {
    run(image, script, Mount::ReadWrite)
}

/// How the guest mounts the copy it makes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mount {
    /// Read-only, and the loop device is read-only too.
    ReadOnly,
    /// Read-write, and the image comes back with what the kernel wrote.
    ReadWrite,
    /// Read-write, and the copy is thrown away.
    ///
    /// The mount ITSELF is the thing being tested — a btrfs mount bumps
    /// the generation and rewrites the superblock whatever else happens
    /// — so an image that came back would no longer match the
    /// `dump-super` report taken straight out of `mkfs.btrfs`, and every
    /// fixture here is compared against exactly that.
    Scratch,
}

#[track_caller]
fn run(image: &str, script: &str, mount: Mount) -> Output {
    session();
    assert!(
        std::path::Path::new(image).starts_with(repo()),
        "the kernel oracle was given {image}, which is outside {}. The guest sees this \
         repository and nothing else of the host.",
        repo().display()
    );

    let run = Run::new();
    let guest = guest_script(image, script, mount, &run);
    let out = guest_shell(&guest)
        .unwrap_or_else(|error| panic!("cannot run the kernel oracle in the guest: {error}"));
    let Some(code) = run.code() else {
        panic!(
            "the kernel oracle could not run in the fs-linux-test-harness VM \
             (the harness exited {:?}). `chore vm:status` shows the VM.\n{}{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let (stdout, stderr) = run.streams();
    println!(
        "[kernel vm] mount -t btrfs -o {} {image} -> {code}",
        match mount {
            Mount::ReadOnly => "ro",
            Mount::ReadWrite => "rw",
            Mount::Scratch => "rw (discarded)",
        }
    );
    Output {
        status: std::os::unix::process::ExitStatusExt::from_raw(code << 8),
        stdout,
        stderr,
    }
}

/// The guest side: copy in, mount, run, unmount, copy back.
///
/// Every step is checked, and the unmount happens on every path — a
/// loop device left attached to an image the next test rewrites is a
/// failure that lands somewhere else entirely.
fn guest_script(image: &str, script: &str, mount: Mount, run: &Run) -> String {
    let options = if mount == Mount::ReadOnly { "ro" } else { "rw" };
    let losetup = if mount == Mount::ReadOnly { "--read-only" } else { "" };
    let copy_back = if mount == Mount::ReadWrite {
        "cp --sparse=always \"$work/image\" $IMG"
    } else {
        "true"
    };
    format!(
        r#"set -euo pipefail
mkdir -p {dir}
IMG={image}
work="$(mktemp -d /var/tmp/fs-btrfs-kernel.XXXXXX)"
mnt="$work/mnt"
mkdir -p "$mnt"
cp --sparse=always "$IMG" "$work/image"
loop="$(losetup --find --show {losetup} "$work/image")"
cleanup() {{
    mountpoint -q "$mnt" && umount "$mnt" || true
    losetup -d "$loop" 2>/dev/null || true
}}
trap cleanup EXIT
mount -t btrfs -o {options} "$loop" "$mnt"
status=0
MNT="$mnt" IMG="$IMG" bash -euo pipefail -c {script} > {stdout} 2> {stderr} || status=$?
# Unmounted BEFORE the copy back: the kernel writes on umount, and an
# image copied while mounted is one no test can reason about.
umount "$mnt"
losetup -d "$loop"
trap - EXIT
if [ "$status" = 0 ]; then
    {copy_back}
fi
printf %s "$status" > {status}
rm -rf "$work""#,
        dir = guest_quote(&run.dir.to_string_lossy()),
        image = guest_quote(image),
        script = guest_quote(script),
        stdout = guest_quote(&run.stdout.to_string_lossy()),
        stderr = guest_quote(&run.stderr.to_string_lossy()),
        status = guest_quote(&run.status.to_string_lossy()),
    )
}

/// [`guest_kernel_read`], with the script's output as text and a failure
/// that prints everything the guest said.
#[track_caller]
pub fn guest_kernel_read_ok(image: &str, what: &str, script: &str) -> String {
    let out = guest_kernel_read(image, script);
    assert!(
        out.status.success(),
        "[{what}] the kernel could not read back {image} ({:?}):\n{}{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// [`guest_kernel_write`], with the same treatment.
#[track_caller]
pub fn guest_kernel_write_ok(image: &str, what: &str, script: &str) -> String {
    let out = guest_kernel_write(image, script);
    assert!(
        out.status.success(),
        "[{what}] the kernel could not write to {image} ({:?}):\n{}{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// What the kernel reported, as `(kind, path) -> value`.
fn parse(report: &str) -> BTreeMap<(String, String), String> {
    report
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut fields = line.splitn(3, '\t');
            let kind = fields.next().unwrap_or_default().to_string();
            let path = fields.next().unwrap_or_default().to_string();
            let value = fields.next().unwrap_or_default().to_string();
            ((kind, path), value)
        })
        .collect()
}

/// The guest script: walk the mounted tree, hash every file, read the
/// metadata this test cares about, and print `kind<TAB>path<TAB>value`.
///
/// One invocation does all of it. The alternative — a guest call per
/// question — is a hundred round trips for one image.
const REPORT: &str = r#"
cd "$MNT"
find . -mindepth 1 -printf '%P\n' | sort | while read -r path; do
    kind="$(stat -c '%F' "$path" | tr ' ' '-')"
    printf 'type\t%s\t%s\n' "$path" "$kind"
    printf 'mode\t%s\t%s\n' "$path" "$(stat -c '%a' "$path")"
    case "$kind" in
        regular-file | regular-empty-file)
            printf 'size\t%s\t%s\n' "$path" "$(stat -c '%s' "$path")"
            printf 'sha256\t%s\t%s\n' "$path" "$(sha256sum "$path" | cut -d' ' -f1)"
            ;;
        symbolic-link)
            printf 'target\t%s\t%s\n' "$path" "$(readlink "$path")"
            ;;
    esac
    names="$(getfattr -h -m '.' --absolute-names "$path" 2>/dev/null | sed -n '2,$p' || true)"
    attrs=""
    for name in $names; do
        value="$(getfattr --only-values -n "$name" "$path" 2>/dev/null | od -An -tx1 | tr -d ' \n')"
        attrs="$attrs${attrs:+,}$name=$value"
    done
    if [ -n "$attrs" ]; then
        printf 'xattrs\t%s\t%s\n' "$path" "$attrs"
    fi
done
printf 'kernel\t\t%s\n' "$(uname -r)"
"#;

/// Mount `image` read-only in the guest and report everything in it:
/// for every path, its type, mode, size, SHA-256, symlink target and
/// extended attributes, keyed by `(kind, path)`.
///
/// ONE GUEST CALL. A test compares this against what it wrote.
#[track_caller]
pub fn guest_kernel_report(image: &str, what: &str) -> BTreeMap<(String, String), String> {
    parse(&guest_kernel_read_ok(image, what, REPORT))
}

/// What one round trip through the in-kernel driver produced.
pub struct KernelProbe {
    /// What the kernel read back out of the file the script wrote.
    pub readback: String,
    /// Every btrfs line the kernel logged above info level DURING this
    /// mount, and nothing from any other.
    pub complaints: Vec<String>,
}

/// Serialises the kernel-log bracket.
///
/// The ring buffer is one buffer for the whole guest, so two probes
/// running at once would each clear the other's evidence and attribute
/// the remainder to the wrong image. Test binaries run one at a time,
/// so a lock inside the process is enough — and the alternative, telling
/// every caller to pass `--test-threads=1`, is a rule nobody can see
/// from the test that depends on it.
static KERNEL_LOG: Mutex<()> = Mutex::new(());

/// MOUNT THE IMAGE READ-WRITE, WRITE A FILE, READ IT BACK, and report
/// what the kernel said about it while doing so.
///
/// This is the gate that decides whether the in-kernel btrfs driver
/// accepts an image at all — a mount that succeeds but cannot do IO is
/// not a pass, and neither is one the kernel completes while logging a
/// warning, because that is exactly the shape of defect a
/// self-consistent parser produces: an image malformed in a way the
/// mount itself tolerated.
///
/// It runs on a copy the guest throws away, so the fixture is unchanged:
/// mounting btrfs read-write bumps the generation and rewrites the
/// superblock whatever else happens, and every fixture here is compared
/// against a `dump-super` taken straight out of `mkfs.btrfs`.
///
/// One guest call: clear the log, mount, write, sync, read, unmount,
/// report.
#[track_caller]
pub fn guest_kernel_probe(image: &str, payload: &str) -> KernelProbe {
    let _bracket = KERNEL_LOG.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let script = format!(
        "dmesg -C\n\
         printf %s {payload} > \"$MNT/probe.txt\"\n\
         sync\n\
         printf 'readback\\t%s\\n' \"$(cat \"$MNT/probe.txt\")\"",
        payload = guest_quote(payload)
    );
    // The complaints have to be read AFTER the unmount, which happens
    // outside the script above, so this is a second call — still inside
    // the lock, so nothing else has touched the buffer in between.
    let out = run(image, &script, Mount::Scratch);
    assert!(
        out.status.success(),
        "the in-kernel btrfs driver REFUSED {image} ({:?}):\n{}{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let readback = String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("readback\t").map(str::to_string))
        .unwrap_or_default();

    // `|| true`: grep exits 1 when it matches nothing, which is the good
    // case here rather than a failure.
    let log = guest_shell("dmesg | grep -Ei 'BTRFS (warning|error|critical|alert|emerg)' || true")
        .unwrap_or_else(|error| panic!("cannot read the guest's kernel log: {error}"));
    assert!(
        log.status.success(),
        "cannot read the guest's kernel log:\n{}",
        String::from_utf8_lossy(&log.stderr)
    );
    KernelProbe {
        readback,
        complaints: String::from_utf8_lossy(&log.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect(),
    }
}
