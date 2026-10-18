//! `fs.btrfs stat` reports what THE KERNEL reports: a volume made by
//! `mkfs.btrfs` (btrfs-progs) in the harness VM, filled there by Linux, and
//! every path put to the guest's own `stat`. Identity, type, permission
//! bits, owner, links, size, allocated blocks, device numbers and all four
//! times to the nanosecond must be the ones the kernel printed, with a
//! symlink reported rather than followed.
//!
//! Nested subvolume identity comes from `btrfs inspect-internal rootid`.
//! The compression property is null when absent, not a guessed algorithm.

mod cli_support;

use cli_support::*;
use fs_btrfs_test_support::{guest_kernel_write, oracle};

/// One line per path: `LABEL|` then the guest's `stat -c FORMAT`.
/// Raw mode in hex, owner, links, size, blocks and their unit, the four
/// times as whole seconds and in the human form whose fraction is the
/// nanoseconds, and the device numbers in hex.
const FORMAT: &str = "%i|%f|%u|%g|%h|%s|%b|%B|%X|%x|%Y|%y|%Z|%z|%W|%w|%t|%T";

fn stat_script(fill: &str, paths: &[&str]) -> String {
    let mut script = format!("cd \"$MNT\"\n{fill}sync\n");
    for path in paths {
        let at = if *path == "/" { "." } else { &path[1..] };
        // `-L` is asked first: `-d` follows a symlink, and following one
        // reads it, which moves its access time after the stat above.
        script.push_str(&format!(
            "printf '%s|' {path}; stat --printf='{FORMAT}|' -- {at}; \
             if [ ! -L {at} ] && [ -d {at} ]; then dir={at}; else dir=$(dirname -- {at}); fi; \
             btrfs inspect-internal rootid \"$dir\"\n"
        ));
    }
    script
}

/// The nanoseconds in a `stat` human time, `2001-02-03 04:05:06.123456789 +0000`.
fn nanos(human: &str) -> String {
    let fraction = human
        .split_once('.')
        .map(|(_, rest)| rest.split(' ').next().unwrap_or(""))
        .unwrap_or_else(|| panic!("no fraction in the guest's time {human:?}"));
    assert_eq!(fraction.len(), 9, "the guest's time {human:?}");
    fraction
        .trim_start_matches('0')
        .parse::<u64>()
        .unwrap_or(0)
        .to_string()
}

fn kind(raw_mode: u32) -> &'static str {
    match raw_mode & 0o170000 {
        0o100000 => "file",
        0o040000 => "dir",
        0o120000 => "symlink",
        0o020000 => "char",
        0o060000 => "block",
        0o010000 => "fifo",
        0o140000 => "socket",
        _ => "unknown",
    }
}

/// Every field the guest printed for each path, against `fs.btrfs stat`.
#[track_caller]
fn assert_agrees(img: &str, report: &str, paths: &[&str]) {
    let mut seen = Vec::new();
    for line in report.lines() {
        let f: Vec<&str> = line.split('|').collect();
        assert_eq!(f.len(), 20, "a guest stat line: {line}");
        let path = f[0];
        seen.push(path.to_string());
        let ours = stdout(&ok(tool("fs.btrfs").args([img, "stat", path])));
        let field = |k: &str| json_field(&ours, k);
        let raw_mode = u32::from_str_radix(f[2], 16).unwrap();
        let what = |k: &str| format!("{path}: {k}\nours:\n{ours}\nthe guest's: {line}");

        assert_eq!(field("path"), path, "{}", what("path"));
        assert_eq!(field("inode"), f[1], "{}", what("inode"));
        assert_eq!(field("type"), kind(raw_mode), "{}", what("type"));
        assert_eq!(
            field("mode"),
            format!("{:04o}", raw_mode & 0o7777),
            "{}",
            what("mode")
        );
        assert_eq!(field("uid"), f[3], "{}", what("uid"));
        assert_eq!(field("gid"), f[4], "{}", what("gid"));
        assert_eq!(field("nlink"), f[5], "{}", what("nlink"));
        assert_eq!(field("size"), f[6], "{}", what("size"));
        let allocated = f[7].parse::<u64>().unwrap() * f[8].parse::<u64>().unwrap();
        assert_eq!(
            field("allocated_bytes"),
            allocated.to_string(),
            "{}",
            what("allocated_bytes")
        );
        for (key, secs, human) in [
            ("atime", f[9], f[10]),
            ("mtime", f[11], f[12]),
            ("ctime", f[13], f[14]),
        ] {
            assert_eq!(field(key), secs, "{}", what(key));
            let nsec = format!("{key}_nsec");
            assert_eq!(field(&nsec), nanos(human), "{}", what(&nsec));
        }
        assert_eq!(field("btime"), f[15], "{}", what("btime"));
        assert_eq!(field("btime_nsec"), nanos(f[16]), "{}", what("btime_nsec"));
        assert!(ours.contains("\"btrfs\": {"), "{ours}");
        assert_eq!(field("subvolume"), f[19], "{}", what("btrfs.subvolume"));
        match kind(raw_mode) {
            "char" | "block" => {
                let major = u32::from_str_radix(f[17], 16).unwrap();
                let minor = u32::from_str_radix(f[18], 16).unwrap();
                assert_eq!(field("major"), major.to_string(), "{}", what("major"));
                assert_eq!(field("minor"), minor.to_string(), "{}", what("minor"));
            }
            "symlink" => assert!(ours.contains("\"target\": "), "{}", what("target")),
            _ => assert!(!ours.contains("\"target\": "), "{}", what("target")),
        }
    }
    assert_eq!(
        seen, paths,
        "the guest printed a line for each path:\n{report}"
    );
}

#[test]
fn fs_btrfs_stat_reports_what_the_kernel_reports() {
    let image = scratch_dir("stat-kernel").join("fs.img");
    std::fs::File::create(&image)
        .unwrap()
        .set_len(256 << 20)
        .unwrap();
    let img = image.to_str().unwrap();
    let made = oracle("mkfs.btrfs")
        .args(["-q", "-f", "-s", "4096", "-n", "16384", img])
        .output();
    assert!(made.status.success(), "mkfs.btrfs: {}", stderr(&made));

    // Nothing reads a file once it is written, so no access time moves
    // between the guest's stat and the unmount that writes the inodes.
    let fill = "mkdir dir\n\
                chmod 1777 dir\n\
                head -c 5000 /dev/urandom > file\n\
                chown 1234:5678 file\n\
                chmod 0640 file\n\
                touch -m -d '2001-02-03 04:05:06.123456789' file\n\
                ln file hard\n\
                : > empty\n\
                printf x > old\n\
                touch -d '1960-01-02 03:04:05.5' old\n\
                ln -s file link\n\
                mkfifo fifo\n\
                mknod chr c 1 3\n\
                mknod blk b 7 300\n\
                ln -s absent dangling\n\
                btrfs subvolume create sub >/dev/null\n\
                btrfs subvolume create sub/nested >/dev/null\n\
                printf inside > sub/nested/file\n\
                ln -s file sub/nested/link\n\
                : > compressed\n\
                btrfs property set compressed compression zstd\n\
                head -c 131072 /dev/zero > compressed\n\
                : > nocow\n\
                chattr +C nocow\n";
    let paths = [
        "/",
        "/dir",
        "/file",
        "/hard",
        "/empty",
        "/old",
        "/link",
        "/fifo",
        "/chr",
        "/blk",
        "/dangling",
        "/sub",
        "/sub/nested",
        "/sub/nested/file",
        "/sub/nested/link",
        "/compressed",
        "/nocow",
    ];
    let out = guest_kernel_write(img, &stat_script(fill, &paths));
    assert!(
        out.status.success(),
        "the kernel could not fill the volume:\n{}{}",
        stdout(&out),
        stderr(&out)
    );
    assert_agrees(img, &stdout(&out), &paths);

    let link = stdout(&ok(tool("fs.btrfs").args([img, "stat", "/link"])));
    assert_eq!(json_field(&link, "type"), "symlink", "{link}");
    assert_eq!(json_field(&link, "target"), "file", "{link}");
    assert_eq!(json_field(&link, "size"), "4", "{link}");
    let compressed = stdout(&ok(tool("fs.btrfs").args([img, "stat", "/compressed"])));
    assert_eq!(
        json_field(&compressed, "compression"),
        "zstd",
        "{compressed}"
    );
    assert!(compressed.contains("\"COMPRESS\""), "{compressed}");
    let empty = stdout(&ok(tool("fs.btrfs").args([img, "stat", "/empty"])));
    assert_eq!(json_field(&empty, "compression"), "null", "{empty}");
    let nocow = stdout(&ok(tool("fs.btrfs").args([img, "stat", "/nocow"])));
    assert!(nocow.contains("\"NODATACOW\""), "{nocow}");
    assert!(nocow.contains("\"NODATASUM\""), "{nocow}");
}
