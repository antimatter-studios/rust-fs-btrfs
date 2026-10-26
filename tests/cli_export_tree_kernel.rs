//! `fs.btrfs export --recursive` against a tree THE KERNEL made (#541): a
//! volume made by `mkfs.btrfs` (btrfs-progs) in the harness VM, filled
//! there by Linux with a tree 33 directories deep, a file with three names,
//! three symlinks (to a file, dangling, and leaving the tree), a fifo,
//! times set with `touch`, and a subvolume with a snapshot of it taken
//! before the subvolume's file changed. The guest's own `lstat`, `readlink`
//! and SHA-256 of every path are the manifest every exported file, link,
//! identity and time is held to, and a data block damaged on disk, found by
//! btrfs-progs, must fail its file alone.

mod cli_support;

use cli_support::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, dump_tree, guest_kernel_write_ok, oracle, sha256_hex,
};
use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

/// How deep the tree goes below `/tree`; its deepest entry,
/// `d1/.../d33/f33`, is at depth 34.
const DEPTH: usize = 33;

/// Fill the volume, then print every path below `tree` as the kernel sees
/// it: `M<TAB>path<TAB>type<TAB>dev<TAB>ino<TAB>nlink<TAB>atime_ns<TAB>
/// mtime_ns<TAB>extra`, extra a file's SHA-256 or a symlink's target in
/// hex. Every file is read before any path is stat'ed, so the access
/// times printed are the ones reading left on disk.
const SCRIPT: &str = r#"cd "$MNT"
mkdir tree
cd tree
p=.
for i in $(seq 1 33); do
    p="$p/d$i"
    mkdir "$p"
    printf 'file %s' "$i" > "$p/f$i"
done
head -c 3145729 /dev/urandom > big
: > empty
printf 'linked content\n' > file
ln file hard
ln file d1/hard2
ln -s big link
ln -s absent dangling
ln -s ../.. d1/up
mkfifo pipe
btrfs subvolume create sub >/dev/null
printf snapshot > sub/file
btrfs subvolume snapshot sub snap >/dev/null
printf live > sub/file
touch -d @981173106.123456789 file
touch -d @1100000000.250000000 d1
touch -d @1200000000.123456789 .
sync
cd "$MNT"
python3 - <<'PY'
import hashlib, os, stat
paths = []
for top, dirs, files in os.walk('tree'):
    dirs.sort()
    for name in sorted(dirs) + sorted(files):
        paths.append(os.path.join(top, name))
sums = {}
for p in paths:
    if stat.S_ISREG(os.lstat(p).st_mode):
        sums[p] = hashlib.sha256(open(p, 'rb').read()).hexdigest()
for p in paths:
    s = os.lstat(p)
    if stat.S_ISLNK(s.st_mode):
        kind, extra = 'symlink', os.readlink(p).encode().hex()
    elif stat.S_ISDIR(s.st_mode):
        kind, extra = 'dir', '-'
    elif stat.S_ISREG(s.st_mode):
        kind, extra = 'file', sums[p]
    else:
        kind, extra = 'other', '-'
    print('M', p, kind, s.st_dev, s.st_ino, s.st_nlink, s.st_atime_ns, s.st_mtime_ns, extra, sep='\t')
PY
sync
"#;

/// One path as the kernel saw it.
#[derive(Debug, Clone)]
struct Seen {
    kind: String,
    identity: (u64, u64),
    nlink: u64,
    atime: i128,
    mtime: i128,
    extra: String,
}

/// A volume the kernel filled, and its manifest by path below `/tree`.
fn source(tag: &str) -> (PathBuf, BTreeMap<String, Seen>) {
    let image = scratch_dir(tag).join("fs.img");
    std::fs::File::create(&image)
        .unwrap()
        .set_len(256 << 20)
        .unwrap();
    let out = oracle("mkfs.btrfs")
        .args(["-q", "-f", "-s", "4096", "-n", "16384"])
        .arg(&image)
        .output();
    assert!(out.status.success(), "mkfs.btrfs: {}", stderr(&out));
    let report = guest_kernel_write_ok(image.to_str().unwrap(), "tree export source", SCRIPT);
    assert_btrfs_check_clean(&image, "kernel tree export source");
    let mut seen = BTreeMap::new();
    for line in report.lines().filter_map(|l| l.strip_prefix("M\t")) {
        let f: Vec<&str> = line.split('\t').collect();
        assert_eq!(f.len(), 8, "a manifest line: {line}");
        let n = |i: usize| {
            f[i].parse::<i128>()
                .unwrap_or_else(|e| panic!("{line}: {e}"))
        };
        let path = f[0]
            .strip_prefix("tree/")
            .unwrap_or_else(|| panic!("{line}"));
        seen.insert(
            path.to_string(),
            Seen {
                kind: f[1].to_string(),
                identity: (n(2) as u64, n(3) as u64),
                nlink: n(4) as u64,
                atime: n(5),
                mtime: n(6),
                extra: f[7].to_string(),
            },
        );
    }
    let files = seen.values().filter(|s| s.kind == "file").count();
    assert_eq!(files, DEPTH + 7, "every file was printed:\n{report}");
    (image, seen)
}

/// `fs.btrfs IMAGE export --recursive FLAGS... /tree DEST`.
fn export_tree(img: &Path, flags: &[&str], dest: &Path) -> std::process::Output {
    tool("fs.btrfs")
        .arg(img)
        .args(["export", "--recursive"])
        .args(flags)
        .arg("/tree")
        .arg(dest)
        .output()
        .unwrap()
}

/// The report's item after its path key, up to the next item's path key.
#[track_caller]
fn item<'a>(report: &'a str, path: &str) -> &'a str {
    let needle = format!("\"path\": \"{path}\"");
    let start = report
        .find(&needle)
        .unwrap_or_else(|| panic!("no item for {path} in:\n{report}"));
    let rest = &report[start + needle.len()..];
    &rest[..rest.find("\"path\": ").unwrap_or(rest.len())]
}

/// Every file below `dest` holds the bytes the kernel digested, skipping
/// `skip`.
#[track_caller]
fn assert_files(dest: &Path, seen: &BTreeMap<String, Seen>, skip: &[&str]) {
    for (path, s) in seen {
        if s.kind != "file" || skip.contains(&path.as_str()) {
            continue;
        }
        let bytes = std::fs::read(dest.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"));
        assert_eq!(
            sha256_hex(&bytes),
            s.extra,
            "{path}: exported versus kernel"
        );
    }
}

#[test]
fn a_kernel_tree_is_exported_with_its_links_identities_and_times() {
    let (image, seen) = source("export-tree-kernel");
    let before = std::fs::read(&image).unwrap();
    let dir = scratch_dir("export-tree-kernel-dest");

    // Without --preserve=links symlinks are refused and the fifo skipped;
    // every file, directory, subvolume and snapshot is exported.
    let plain = dir.join("plain");
    let out = export_tree(&image, &[], &plain);
    let report = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{report}{}", stderr(&out));
    let entries = seen.len() + 1;
    assert_eq!(
        json_field(&report, "completed"),
        (entries - 4).to_string(),
        "{report}"
    );
    assert_eq!(json_field(&report, "refused"), "3", "{report}");
    assert_eq!(json_field(&report, "skipped"), "1", "{report}");
    assert_eq!(json_field(&report, "failed"), "0", "{report}");
    assert_files(&plain, &seen, &[]);
    for (path, s) in &seen {
        let local = plain.join(path);
        match s.kind.as_str() {
            "dir" => assert!(local.is_dir(), "{path}"),
            "symlink" | "other" => {
                assert!(std::fs::symlink_metadata(&local).is_err(), "{path}");
                let status = if s.kind == "other" {
                    "skipped"
                } else {
                    "refused"
                };
                let entry = item(&report, &format!("/tree/{path}"));
                assert!(
                    entry.contains(&format!("\"status\": \"{status}\"")),
                    "{entry}"
                );
            }
            _ => {}
        }
    }
    let mut deepest = PathBuf::new();
    for i in 1..=DEPTH {
        deepest.push(format!("d{i}"));
    }
    assert!(plain.join(&deepest).join(format!("f{DEPTH}")).is_file());

    // Links and times preserved, held to the kernel's lstat and readlink.
    let dest = dir.join("preserved");
    let out = export_tree(&image, &["--preserve=links,times"], &dest);
    let report = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "the fifo is skipped: {report}");
    assert_eq!(
        json_field(&report, "completed"),
        (entries - 1).to_string(),
        "{report}"
    );
    assert_eq!(json_field(&report, "skipped"), "1", "{report}");
    assert_eq!(json_field(&report, "refused"), "0", "{report}");
    assert_eq!(json_field(&report, "failed"), "0", "{report}");
    // Every stat before any read, so reading cannot move an access time.
    let local: BTreeMap<&String, std::fs::Metadata> = seen
        .iter()
        .filter(|(_, s)| s.kind != "other")
        .map(|(path, _)| (path, std::fs::symlink_metadata(dest.join(path)).unwrap()))
        .collect();
    let mut by_identity: BTreeMap<(u64, u64), Vec<&String>> = BTreeMap::new();
    for (path, meta) in &local {
        let s = &seen[*path];
        if s.kind == "symlink" {
            let target = std::fs::read_link(dest.join(path)).unwrap();
            let hex: String = target
                .as_os_str()
                .as_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            assert_eq!(hex, s.extra, "{path}: target versus kernel readlink");
            continue;
        }
        let atime = i128::from(meta.atime()) * 1_000_000_000 + i128::from(meta.atime_nsec());
        let mtime = i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec());
        assert_eq!(atime, s.atime, "{path}: access time versus kernel");
        assert_eq!(mtime, s.mtime, "{path}: modification time versus kernel");
        if s.kind == "file" {
            assert_eq!(meta.nlink(), s.nlink, "{path}: link count versus kernel");
            by_identity.entry(s.identity).or_default().push(path);
        }
    }
    // One exported inode per kernel (device, inode), and only one: the
    // snapshot's file shares the subvolume's inode number on another device.
    let mut exported = BTreeMap::new();
    for (identity, paths) in &by_identity {
        let inos: Vec<u64> = paths.iter().map(|p| local[*p].ino()).collect();
        assert!(inos.iter().all(|i| *i == inos[0]), "{paths:?}: one file");
        if let Some(other) = exported.insert(inos[0], identity) {
            panic!("{paths:?} share an exported inode with {other:?}");
        }
    }
    assert!(
        by_identity.values().any(|paths| paths.len() == 3),
        "file, hard and d1/hard2 are one inode: {by_identity:?}"
    );
    assert_eq!(seen["sub/file"].identity.1, seen["snap/file"].identity.1);
    assert_ne!(seen["sub/file"].identity.0, seen["snap/file"].identity.0);
    assert_files(&dest, &seen, &[]);
    assert_eq!(std::fs::read(dest.join("sub/file")).unwrap(), b"live");
    assert_eq!(std::fs::read(dest.join("snap/file")).unwrap(), b"snapshot");

    // The depth bound: an exact fit at 34 is complete, one less fails the
    // deepest file alone.
    let deepest_file = format!("/tree/{}/f{DEPTH}", deepest.display());
    let fit = dir.join("fit");
    let out = export_tree(&image, &["--preserve=links", "--max-depth", "34"], &fit);
    assert_eq!(json_field(&stdout(&out), "failed"), "0", "{}", stdout(&out));
    let over = dir.join("over");
    let out = export_tree(&image, &["--preserve=links", "--max-depth", "33"], &over);
    let report = stdout(&out);
    assert_eq!(json_field(&report, "failed"), "1", "{report}");
    assert!(item(&report, &deepest_file).contains("maximum depth exceeded"));
    assert!(over.join(&deepest).is_dir());
    assert!(!over.join(&deepest).join(format!("f{DEPTH}")).exists());

    assert_eq!(
        std::fs::read(&image).unwrap(),
        before,
        "export changed the image"
    );
}

#[test]
fn a_damaged_file_fails_alone_and_is_never_published_or_replaced() {
    let (image, seen) = source("export-tree-damaged");
    // btrfs-progs locates big's data and its copies; no parser of this
    // crate chooses the damaged byte. The rest of the tree is inline.
    let dump = dump_tree(&image, "5");
    let logical: u64 = dump
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("extent data disk byte ")?
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
                .filter(|n| *n != 0)
        })
        .expect("the kernel's big file has a regular data extent");
    let mapped = oracle("btrfs-map-logical")
        .args(["-l", &logical.to_string()])
        .arg(&image)
        .output();
    assert!(mapped.status.success(), "{}", stderr(&mapped));
    let copies: Vec<u64> = stdout(&mapped)
        .lines()
        .filter(|line| line.starts_with("mirror"))
        .filter_map(|line| {
            let words: Vec<_> = line.split_whitespace().collect();
            let at = words.iter().position(|word| *word == "physical")?;
            words.get(at + 1)?.parse().ok()
        })
        .collect();
    assert!(!copies.is_empty(), "{}", stdout(&mapped));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&image)
        .unwrap();
    for physical in copies {
        let mut byte = [0];
        file.read_exact_at(&mut byte, physical).unwrap();
        byte[0] ^= 0xff;
        file.write_all_at(&byte, physical).unwrap();
    }
    drop(file);
    let checked = oracle("btrfs")
        .args(["check", "--readonly", "--check-data-csum"])
        .arg(&image)
        .output();
    assert!(
        !checked.status.success(),
        "the independent checker must detect the damaged data"
    );

    let dest = scratch_dir("export-tree-damaged-dest").join("tree");
    for replace in [false, true] {
        let flags: &[&str] = if replace {
            &["--preserve=links", "--replace"]
        } else {
            &["--preserve=links"]
        };
        let out = export_tree(&image, flags, &dest);
        let report = stdout(&out);
        assert_eq!(out.status.code(), Some(1), "{report}{}", stderr(&out));
        if !replace {
            assert_eq!(json_field(&report, "failed"), "1", "{report}");
        }
        assert!(item(&report, "/tree/big").contains("\"status\": \"failed\""));
        assert_files(&dest, &seen, &["big"]);
        if replace {
            assert_eq!(
                std::fs::read(dest.join("big")).unwrap(),
                b"original destination",
                "a failed replacement kept the original bytes"
            );
        } else {
            assert!(
                std::fs::symlink_metadata(dest.join("big")).is_err(),
                "an incomplete file was published"
            );
            std::fs::write(dest.join("big"), b"original destination").unwrap();
        }
        let staged: Vec<_> = std::fs::read_dir(&dest)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.as_bytes().starts_with(b"."))
            .collect();
        assert!(staged.is_empty(), "staging left behind: {staged:?}");
    }
}
