//! `fs.btrfs import --preserve` and `export --preserve=xattrs` (#435, #440),
//! judged by the Linux kernel in the harness VM and `btrfs check`, never by
//! what the tool reports about itself.
//!
//! - a tree with a three-name hard link, relative and absolute symbolic
//!   links, set times and an extended attribute is imported, and the kernel
//!   reads a link count of 3 on one inode, the targets byte-identical, the
//!   times equal to the nanosecond and the attribute's bytes;
//! - an attribute the kernel set on a file comes out of `export` with the
//!   same bytes on the host.

#![cfg(target_os = "linux")]

mod cli_support;

use cli_support::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, guest_kernel_read_ok, guest_kernel_write_ok, oracle,
};
use std::fs::{self, File, FileTimes};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ACCESSED: (u64, u32) = (1_400_000_000, 500_000_000);
const MODIFIED: (u64, u32) = (1_500_000_000, 123_456_789);

fn at((sec, nsec): (u64, u32)) -> SystemTime {
    UNIX_EPOCH + Duration::new(sec, nsec)
}

fn volume(dir: &Path) -> PathBuf {
    let image = dir.join("preserve.img");
    File::create(&image)
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
    image
}

/// Set an extended attribute on a host file, through Python's `os.setxattr`
/// because the standard library has no call for it.
fn set_host_xattr(path: &Path, name: &str, hex: &str) {
    let done = Command::new("python3")
        .args([
            "-c",
            "import os,sys; os.setxattr(sys.argv[1], sys.argv[2], bytes.fromhex(sys.argv[3]))",
        ])
        .arg(path)
        .args([name, hex])
        .output()
        .unwrap();
    assert!(
        done.status.success(),
        "setxattr: {}",
        String::from_utf8_lossy(&done.stderr)
    );
}

#[test]
fn a_tree_imported_with_preserve_keeps_its_links_times_and_attributes() {
    let dir = scratch_dir("preserve-import");
    let image = volume(&dir);

    let tree = dir.join("tree");
    fs::create_dir_all(tree.join("sub")).unwrap();
    fs::write(tree.join("a.txt"), b"one inode\n").unwrap();
    fs::hard_link(tree.join("a.txt"), tree.join("b.txt")).unwrap();
    fs::hard_link(tree.join("a.txt"), tree.join("sub/c.txt")).unwrap();
    std::os::unix::fs::symlink("a.txt", tree.join("relative")).unwrap();
    std::os::unix::fs::symlink("/not/on/this/volume", tree.join("absolute")).unwrap();
    fs::write(tree.join("attr.txt"), b"attributes\n").unwrap();
    set_host_xattr(&tree.join("attr.txt"), "user.note", "00ff6e6f7465");
    let times = FileTimes::new()
        .set_accessed(at(ACCESSED))
        .set_modified(at(MODIFIED));
    File::options()
        .write(true)
        .open(tree.join("attr.txt"))
        .unwrap()
        .set_times(times)
        .unwrap();

    let out = ok(tool("fs.btrfs")
        .arg(&image)
        .args(["import", "--recursive", "--preserve=times,links,xattrs"])
        .arg(&tree)
        .arg("/tree"));
    let report = stdout(&out);
    for count in ["failed", "refused"] {
        assert_eq!(json_field(&report, count), "0", "{report}");
    }

    assert_btrfs_check_clean(&image, "after import --preserve");
    let seen = guest_kernel_read_ok(
        image.to_str().unwrap(),
        "the imported tree",
        r#"cd "$MNT/tree"
python3 - <<'PY'
import os
inodes = {os.stat(n).st_ino for n in ('a.txt', 'b.txt', 'sub/c.txt')}
print('inodes', len(inodes), 'links', os.stat('a.txt').st_nlink)
print('relative', os.readlink('relative'))
print('absolute', os.readlink('absolute'))
s = os.stat('attr.txt')
print('times', s.st_atime_ns, s.st_mtime_ns)
print('attr', os.getxattr('attr.txt', 'user.note').hex())
PY
"#,
    );
    let ns = |(sec, nsec): (u64, u32)| u128::from(sec) * 1_000_000_000 + u128::from(nsec);
    assert_eq!(
        seen,
        format!(
            "inodes 1 links 3\nrelative a.txt\nabsolute /not/on/this/volume\ntimes {} {}\nattr 00ff6e6f7465\n",
            ns(ACCESSED),
            ns(MODIFIED)
        )
    );
}

#[test]
fn an_attribute_the_kernel_set_is_exported_with_its_bytes() {
    let dir = scratch_dir("preserve-export");
    let image = volume(&dir);
    guest_kernel_write_ok(
        image.to_str().unwrap(),
        "the kernel sets an attribute",
        r#"mkdir "$MNT/tree"
echo attributes > "$MNT/tree/attr.txt"
python3 -c "import os,sys; os.setxattr(sys.argv[1], 'user.note', bytes.fromhex('00ff6e6f7465'))" "$MNT/tree/attr.txt"
sync
"#,
    );

    let dest = dir.join("exported");
    let out = ok(tool("fs.btrfs")
        .arg(&image)
        .args(["export", "--recursive", "--preserve=xattrs", "/tree"])
        .arg(&dest));
    let report = stdout(&out);
    assert_eq!(json_field(&report, "failed"), "0", "{report}");

    let read = Command::new("python3")
        .args([
            "-c",
            "import os,sys; print(os.getxattr(sys.argv[1], 'user.note').hex())",
        ])
        .arg(dest.join("attr.txt"))
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&read.stdout).trim(),
        "00ff6e6f7465",
        "{}",
        String::from_utf8_lossy(&read.stderr)
    );
}
