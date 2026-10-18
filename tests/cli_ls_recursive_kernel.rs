//! GNU find over the guest kernel's own mount is the oracle, including
//! nested subvolumes and a snapshot stored inside its own ancestor.
//!
//! Copies of that kernel-made volume are then damaged where no kernel
//! writes: one directory's `DIR_INDEX` entry is pointed at a directory
//! already listed, another's at the root above it, and a third's is given
//! a name longer than any name may be. Each block is rewritten with a fresh
//! checksum in every mirror, so the walk meets the damage itself rather
//! than a checksum failure. The walk must still end: a directory met again
//! is listed once as revisited and not entered, an unreadable one stops the
//! walk with exit 1 and everything before it on stdout, and no run changes
//! a byte of its image. What the damage leaves intact is still held to
//! what find printed.
mod cli_support;

use cli_support::*;
use fs_btrfs::dir::{offsets, DIR_INDEX_KEY, MAX_NAME_LEN};
use fs_btrfs::Filesystem;
use fs_btrfs_test_support::{guest_kernel_write, oracle, sha256_hex};
use fs_core::FileDevice;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{Arc, OnceLock};

/// What find printed for each path: its type, size and inode number.
type Found = BTreeMap<Vec<u8>, (String, u64, u64)>;

/// Bounds the undamaged tree fits well inside, so a walk that loops on
/// damage fails on a bound instead of hanging.
const BOUNDS: [&str; 4] = ["--limit", "64", "--max-depth", "12"];

/// The inode number of every subvolume's root directory, the top level's
/// included.
const SUBVOLUME_ROOT: u64 = 256;

fn rows(report: &str) -> Vec<(Vec<u8>, &str)> {
    report
        .split("\n  {")
        .skip(1)
        .map(|row| {
            let path = if row.contains("\"path_hex\"") {
                let hex = json_field(row, "path_hex");
                (0..hex.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                    .collect()
            } else {
                json_field(row, "path").into_bytes()
            };
            (path, row)
        })
        .collect()
}

fn components(path: &[u8]) -> Vec<&[u8]> {
    path.split(|b| *b == b'/')
        .filter(|c| !c.is_empty())
        .collect()
}

/// The listed rows as find's fields, each at the depth its path implies
/// and each path once.
fn found(listed: &[(Vec<u8>, &str)]) -> Found {
    let mut actual = BTreeMap::new();
    for (path, row) in listed {
        assert_eq!(json_field(row, "depth"), components(path).len().to_string());
        assert!(actual
            .insert(
                path.clone(),
                (
                    json_field(row, "type"),
                    json_field(row, "size").parse::<u64>().unwrap(),
                    json_field(row, "inode").parse::<u64>().unwrap()
                )
            )
            .is_none());
    }
    actual
}

/// The kernel-made volume and find's walk of it, made once for every test
/// here.
fn kernel_tree() -> &'static (PathBuf, Found) {
    static TREE: OnceLock<(PathBuf, Found)> = OnceLock::new();
    TREE.get_or_init(build_kernel_tree)
}

fn build_kernel_tree() -> (PathBuf, Found) {
    let image = scratch_dir("recursive-kernel").join("fs.img");
    std::fs::File::create(&image)
        .unwrap()
        .set_len(256 << 20)
        .unwrap();
    let img = image.to_str().unwrap();
    let made = oracle("mkfs.btrfs")
        .args(["-q", "-f", "-s", "4096", "-n", "16384", img])
        .output();
    assert!(made.status.success(), "mkfs.btrfs: {}", stderr(&made));
    let out = guest_kernel_write(
        img,
        r#"cd "$MNT"
mkdir -p a/deep/root empty z/alias
printf payload > a/deep/file
printf raw > $'a/deep/raw-\xff'
printf space > 'two words'
ln a/deep/file hard
ln -s a dirlink
ln -s ../.. a/deep/up
ln -s absent dangling
mkfifo fifo
btrfs subvolume create sub >/dev/null
btrfs subvolume create sub/inner >/dev/null
btrfs subvolume create sub/other >/dev/null
printf nested > sub/inner/file
printf second > sub/other/file
btrfs subvolume snapshot sub sub/self >/dev/null
sync
find . -mindepth 1 -printf '/%P\0%y\0%s\0%i\0'
"#,
    );
    assert!(
        out.status.success(),
        "kernel tree: {}{}",
        stdout(&out),
        stderr(&out)
    );
    let fields: Vec<&[u8]> = out.stdout.split(|b| *b == 0).collect();
    assert_eq!(fields.last(), Some(&&b""[..]));
    assert_eq!((fields.len() - 1) % 4, 0);
    let number = |b: &[u8]| std::str::from_utf8(b).unwrap().parse::<u64>().unwrap();
    let mut expected = BTreeMap::new();
    for f in fields[..fields.len() - 1].chunks_exact(4) {
        let kind = match f[1] {
            b"d" => "dir",
            b"f" => "file",
            b"l" => "symlink",
            b"p" => "fifo",
            other => panic!("find type {other:?}"),
        };
        assert!(expected
            .insert(
                f[0].to_vec(),
                (kind.to_string(), number(f[2]), number(f[3]))
            )
            .is_none());
    }
    assert!(expected.len() > 20, "find must walk the whole tree");
    (image, expected)
}

/// A copy of the kernel-made volume in which directory `dir` of the
/// subvolume at `subvolume` has the `DIR_INDEX` entry for `name` changed
/// by `edit`, its block rewritten with a fresh checksum in every mirror.
fn damaged(tag: &str, subvolume: &str, dir: u64, name: &str, edit: impl Fn(&mut [u8])) -> PathBuf {
    let copy = scratch_dir(&format!("recursive-{tag}")).join("fs.img");
    std::fs::copy(&kernel_tree().0, &copy).unwrap();
    let tree = Filesystem::mount(Arc::new(FileDevice::open(copy.to_str().unwrap()).unwrap()))
        .unwrap()
        .subvolume_at(subvolume.as_bytes())
        .unwrap();
    edit_item(
        &copy,
        tree,
        |k, d| {
            k.objectid == dir
                && k.key_type == DIR_INDEX_KEY
                && d.get(offsets::NAME..) == Some(name.as_bytes())
        },
        edit,
    );
    copy
}

/// An entry's edit pointing it at inode `ino` of the same tree.
fn retarget(ino: u64) -> impl Fn(&mut [u8]) {
    move |d: &mut [u8]| {
        d[offsets::LOCATION..offsets::LOCATION + 8].copy_from_slice(&ino.to_le_bytes())
    }
}

/// The bounded walk of `image` from its root, which must not change a
/// byte of it.
fn walk(image: &Path) -> Output {
    let before = sha256_hex(&std::fs::read(image).unwrap());
    let out = tool("fs.btrfs")
        .arg(image)
        .args(["ls", "-R"])
        .args(BOUNDS)
        .arg("/")
        .output()
        .unwrap();
    assert_eq!(
        sha256_hex(&std::fs::read(image).unwrap()),
        before,
        "ls -R changed {}",
        image.display()
    );
    out
}

#[test]
fn recursive_ls_matches_find_across_subvolumes_and_an_ancestor_snapshot() {
    let (image, expected) = kernel_tree();
    let img = image.to_str().unwrap();
    let report = stdout(&ok(tool("fs.btrfs").args([img, "ls", "--recursive", "/"])));
    let listed = rows(&report);
    for (path, row) in &listed {
        assert!(
            !row.contains("\"revisited\": true"),
            "{}",
            String::from_utf8_lossy(path)
        );
    }
    let actual = found(&listed);
    assert_eq!(
        &actual, expected,
        "recursive ls must equal the kernel's find"
    );
    let order: Vec<_> = listed.iter().map(|(p, _)| components(p)).collect();
    let mut sorted = order.clone();
    sorted.sort();
    assert_eq!(
        order, sorted,
        "depth first with each directory sorted as bytes"
    );
    assert!(listed.iter().all(|(p, _)| !p.starts_with(b"/dirlink/")));
    for path in [
        "/sub",
        "/sub/inner",
        "/sub/other",
        "/sub/self",
        "/sub/self/inner",
        "/sub/self/other",
    ] {
        let (_, row) = listed.iter().find(|(p, _)| p == path.as_bytes()).unwrap();
        assert_eq!(json_field(row, "subvolume"), "true", "{path}");
    }
    // A snapshot's copied subvolume entries have no ROOT_REF in its tree.
    // The kernel shows empty inode-2 placeholders, not the source trees.
    for path in ["/sub/self/inner", "/sub/self/other"] {
        assert_eq!(actual[path.as_bytes()].2, 2, "{path}");
        assert!(listed
            .iter()
            .all(|(p, _)| !p.starts_with(format!("{path}/").as_bytes())));
    }
    assert_eq!(
        report,
        stdout(&ok(tool("fs.btrfs").args([img, "ls", "-R", "/"])))
    );
    let subtree = stdout(&ok(tool("fs.btrfs").args([img, "ls", "-R", "/sub"])));
    assert!(rows(&subtree)
        .iter()
        .any(|(p, r)| p == b"/sub/inner/file" && json_field(r, "depth") == "2"));
    let out = walk(image);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        report,
        "the damaged copies' bounds fit the undamaged tree"
    );
}

#[test]
fn a_directory_linked_twice_or_inside_itself_is_listed_once_more_as_revisited() {
    let (_, expected) = kernel_tree();
    let ino = |path: &str| expected[path.as_bytes()].2;
    // The cycle points at the top level's root, inode 256 of tree 5. /sub,
    // /sub/inner, /sub/other and /sub/self are inode 256 of their own
    // trees, so they are revisits only to a walk that leaves the tree out
    // of a directory's identity.
    for (tag, dir, name, target) in [
        ("twice", "/z", "alias", ino("/a")),
        ("cycle", "/a/deep", "root", SUBVOLUME_ROOT),
    ] {
        let alias = format!("{dir}/{name}");
        let image = damaged(tag, "/", ino(dir), name, retarget(target));
        let out = walk(&image);
        assert!(
            out.status.success(),
            "{tag}: a revisit ends the walk cleanly: {}",
            stderr(&out)
        );
        let report = stdout(&out);
        let listed = rows(&report);
        let revisited: Vec<_> = listed
            .iter()
            .filter(|(_, row)| row.contains("\"revisited\": true"))
            .map(|(path, _)| String::from_utf8_lossy(path))
            .collect();
        assert_eq!(
            revisited,
            [alias.as_str()],
            "{tag}: only {alias} is revisited"
        );
        let (_, row) = listed
            .iter()
            .find(|(path, _)| path == alias.as_bytes())
            .unwrap();
        assert_eq!(json_field(row, "type"), "dir", "{tag}: {alias}");
        assert_eq!(
            json_field(row, "inode"),
            target.to_string(),
            "{tag}: {alias}"
        );
        assert!(
            listed
                .iter()
                .all(|(path, _)| !path.starts_with(format!("{alias}/").as_bytes())),
            "{tag}: {alias} is not entered again"
        );
        let mut actual = found(&listed);
        actual.remove(alias.as_bytes());
        let mut intact = expected.clone();
        intact.remove(alias.as_bytes());
        assert_eq!(actual, intact, "{tag}: everything else is the kernel's");
    }
}

#[test]
fn an_unreadable_directory_stops_the_walk_keeping_the_entries_before_it() {
    let (_, expected) = kernel_tree();
    // /sub/inner is a subvolume part way through the walk, so entries come
    // before it, inside its own tree and after it.
    let image = damaged("unreadable", "/sub/inner", SUBVOLUME_ROOT, "file", |d| {
        let past_the_limit = MAX_NAME_LEN as u16 + 45;
        d[offsets::NAME_LEN..offsets::NAME_LEN + 2].copy_from_slice(&past_the_limit.to_le_bytes())
    });
    let out = walk(&image);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert_eq!(json_field(&err, "code"), "1", "{err}");
    assert!(
        json_field(&err, "error").starts_with("/sub/inner: "),
        "the error names the unreadable directory: {err}"
    );
    let mut order: Vec<&Vec<u8>> = expected.keys().collect();
    order.sort_by(|a, b| components(a).cmp(&components(b)));
    let stop = order
        .iter()
        .position(|path| path.as_slice() == b"/sub/inner")
        .unwrap();
    assert!(stop + 1 < order.len(), "entries come after /sub/inner");
    let report = stdout(&out);
    let listed = rows(&report);
    assert_eq!(
        listed.iter().map(|(path, _)| path).collect::<Vec<_>>(),
        order[..=stop],
        "stdout keeps exactly the entries walked before the stop, in order"
    );
    let mut kept = expected.clone();
    kept.retain(|path, _| order[..=stop].contains(&path));
    assert_eq!(found(&listed), kept, "each kept entry is the kernel's");
}
