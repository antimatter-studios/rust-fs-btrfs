//! `fs.btrfs mkdir --parents`, judged by THE KERNEL and by btrfs-progs: a
//! volume made by `mkfs.btrfs` in the harness VM and filled there by Linux,
//! a chain made by the guest's own `mkdir -p` among what it holds, has
//! chains made by `fs.btrfs`: a new one from the root, one under a chain
//! already there, one that `..` passes through, and one that passes into a
//! subvolume and back out of it. `btrfs check --readonly` must call it
//! clean, and the kernel's own mount must show each directory made with
//! the type, mode and link count of its twin in the guest's chain, each
//! parent's link count moved as the guest's own chain moved it, and every
//! other name as it was. A chain that is already there, through a
//! subvolume included, is success, and the image is not written at all.
//!
//! The second test puts every refusal to the guest's `mkdir -p` first, on
//! the kernel's mount, and `fs.btrfs` must refuse the same paths for the
//! same reason, leaving the image byte-identical: the reasons are measured
//! on Btrfs here, not carried over from another filesystem. Where the
//! guest follows a symlink on the way, `fs.btrfs` follows none, so a
//! symlink to a directory and a dangling one with more path after it are
//! not compared; nor is a directory inside a subvolume other than the top
//! level, which the kernel makes and `fs.btrfs` refuses with exit 3.
//! `tests/cli/test-write.sh` holds what `fs.btrfs` does with them.

mod cli_support;

use cli_support::*;
use fs_btrfs_test_support::{
    assert_btrfs_check_clean, guest_kernel_read, guest_kernel_write, oracle,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Every path on the kernel's mount, the root included, one line each:
/// `path|type|links|device:inode|mode|content`, the content being a
/// regular file's SHA-256 and a symlink's target. The device is part of
/// the identity because every subvolume numbers its inodes afresh.
const LISTING: &str = r#"find . | LC_ALL=C sort | while read -r p; do
    case "$(stat -c %F -- "$p")" in
        regular*) content="$(sha256sum <"$p" | cut -d' ' -f1)" ;;
        symbolic*) content="$(readlink -- "$p")" ;;
        *) content=- ;;
    esac
    q="${p#.}"
    printf '%s|%s|%s\n' "${q:-/}" "$(stat -c '%F|%h|%d:%i|%a' -- "$p")" "$content"
done
"#;

/// A path's line from [`LISTING`]: type, links, identity, mode, content.
#[derive(Clone, Debug, PartialEq)]
struct Seen {
    kind: String,
    links: u64,
    inode: String,
    mode: String,
    content: String,
}

fn listing(out: &std::process::Output) -> BTreeMap<String, Seen> {
    assert!(
        out.status.success(),
        "the kernel could not mount and list the volume:\n{}{}",
        stdout(out),
        stderr(out)
    );
    stdout(out)
        .lines()
        .map(|line| {
            let f: Vec<&str> = line.split('|').collect();
            assert_eq!(f.len(), 6, "a guest listing line: {line}");
            let seen = Seen {
                kind: f[1].to_string(),
                links: f[2].parse().unwrap(),
                inode: f[3].to_string(),
                mode: f[4].to_string(),
                content: f[5].to_string(),
            };
            (f[0].to_string(), seen)
        })
        .collect()
}

fn fresh_volume(tag: &str) -> String {
    let image = scratch_dir(tag).join("fs.img");
    std::fs::File::create(&image)
        .unwrap()
        .set_len(256 << 20)
        .unwrap();
    let img = image.to_str().unwrap().to_string();
    let made = oracle("mkfs.btrfs")
        .args(["-q", "-f", "-s", "4096", "-n", "16384", &img])
        .output();
    assert!(made.status.success(), "mkfs.btrfs: {}", stderr(&made));
    img
}

/// The strings of the report's `created` array, in order.
#[track_caller]
fn created(report: &str) -> Vec<String> {
    let needle = "\"created\": [";
    let start = report
        .find(needle)
        .unwrap_or_else(|| panic!("no created array in:\n{report}"))
        + needle.len();
    let end = start + report[start..].find(']').expect("the array closes");
    report[start..end]
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

#[test]
fn mkdir_parents_makes_chains_the_kernel_then_sees() {
    let img = fresh_volume("mkdir-p-kernel");
    let fill = format!(
        "cd \"$MNT\"\n\
         umask 022\n\
         mkdir -p have/deep twin/a/b/c\n\
         head -c 5000 /dev/urandom > have/file\n\
         ln -s have dirlink\n\
         btrfs subvolume create sub >/dev/null\n\
         mkdir -p sub/inner\n\
         sync\n\
         {LISTING}"
    );
    let before = listing(&guest_kernel_write(&img, &fill));
    // What one subdirectory adds to its parent's link count, measured on
    // the guest's own chain rather than assumed: `/twin/a/b` holds one
    // subdirectory and `/twin/a/b/c` none.
    let per_subdir = before["/twin/a/b"].links - before["/twin/a/b/c"].links;

    // Each chain, the directories it must make, and the directory of the
    // guest's chain each one must look like.
    let chains: [(&str, &[(&str, &str)]); 4] = [
        (
            "/ours/a/b/c",
            &[
                ("/ours", "/twin"),
                ("/ours/a", "/twin/a"),
                ("/ours/a/b", "/twin/a/b"),
                ("/ours/a/b/c", "/twin/a/b/c"),
            ],
        ),
        (
            "/have/deep/x/y",
            &[
                ("/have/deep/x", "/twin/a/b"),
                ("/have/deep/x/y", "/twin/a/b/c"),
            ],
        ),
        (
            "/up/../down",
            &[("/up", "/twin/a/b/c"), ("/up/../down", "/twin/a/b/c")],
        ),
        ("/sub/../viasub", &[("/sub/../viasub", "/twin/a/b/c")]),
    ];
    for (path, made) in chains {
        let report = stdout(&ok(tool("fs.btrfs").args([
            &img,
            "mkdir",
            "--parents",
            path,
        ])));
        assert_eq!(json_field(&report, "path"), path, "{report}");
        let names: Vec<&str> = made.iter().map(|(name, _)| *name).collect();
        assert_eq!(created(&report), names, "{report}");
    }

    // A chain already there, all the way: success, nothing made, and the
    // image not written. A subvolume's top is a directory on the way.
    let image = std::fs::read(&img).expect("read the image");
    for path in [
        "/twin/a/b/c",
        "/have/deep/x/y",
        "/have/./deep/..",
        "/",
        "/sub",
        "/sub/inner",
        "/sub/../have",
    ] {
        let report = stdout(&ok(tool("fs.btrfs").args([&img, "mkdir", "-p", path])));
        assert_eq!(json_field(&report, "path"), path, "{report}");
        assert!(created(&report).is_empty(), "{path}: {report}");
        assert!(
            std::fs::read(&img).expect("read the image") == image,
            "mkdir -p {path}, which was all there, wrote to the image"
        );
    }
    let dirty = stdout(&ok(tool("fs.btrfs").args([&img, "get", "dirty", "--text"])));
    assert_eq!(dirty.trim(), "false", "mkdir -p left the volume dirty");

    assert_btrfs_check_clean(Path::new(&img), "the volume fs.btrfs mkdir --parents left");

    let after = listing(&guest_kernel_read(&img, &format!("cd \"$MNT\"\n{LISTING}")));
    // Every directory made is a directory the kernel sees as its twin in
    // the guest's chain: the same type, mode and links, and an identity of
    // its own. `/up/../down` is `/down` on the kernel's mount, and
    // `/sub/../viasub` is `/viasub`.
    let mut expected = before.clone();
    for (_, made) in chains {
        for (name, twin) in made {
            let shown = name.replace("/up/../", "/").replace("/sub/../", "/");
            let seen = after
                .get(&shown)
                .unwrap_or_else(|| panic!("the kernel does not see {shown}: {after:?}"));
            assert_eq!(
                (&seen.kind, seen.links, &seen.mode),
                (
                    &before[*twin].kind,
                    before[*twin].links,
                    &before[*twin].mode
                ),
                "{shown} (left) is not like the guest's {twin} (right)"
            );
            assert!(
                before.values().all(|b| b.inode != seen.inode),
                "{shown} reuses an inode that was in use: {seen:?}"
            );
            expected.insert(shown, seen.clone());
        }
    }
    let identities: BTreeSet<&str> = after.values().map(|s| s.inode.as_str()).collect();
    assert_eq!(
        identities.len(),
        after.len(),
        "two paths share an inode: {after:?}"
    );
    // The root gained /ours, /up, /down and /viasub; /have/deep gained x.
    expected.get_mut("/").unwrap().links += 4 * per_subdir;
    expected.get_mut("/have/deep").unwrap().links += per_subdir;
    assert_eq!(
        after, expected,
        "the kernel's view after mkdir --parents (left) is not the view before it plus the \
         directories made (right)"
    );
}

/// Our message for the kernel's reason: the text strerror gives the errno
/// the guest's `mkdir -p` reported.
fn ours(reason: &str) -> &'static str {
    match reason {
        "File exists" => "already exists",
        "Not a directory" => "not a directory",
        "File name too long" => "name too long",
        "No such file or directory" => "no such file or directory",
        "NOT REFUSED" => panic!("the kernel did not refuse it"),
        other => panic!("no message is mapped for the kernel's {other:?}"),
    }
}

#[test]
fn mkdir_parents_refuses_what_the_kernel_refuses() {
    let img = fresh_volume("mkdir-p-kernel-refusals");
    let long = "n".repeat(256);
    // Each path, as fs.btrfs is given it; the guest is given it without the
    // leading `/`, from the root of its mount. Where two reasons apply the
    // kernel names the first name on the way that fails, so `/file/<long>`
    // is not a directory rather than a name too long. Inside the subvolume
    // `sub`, the refusals the kernel gives there.
    let cases = [
        "/file".to_string(),
        "/file/".to_string(),
        "/file/x".to_string(),
        "/file/x/y".to_string(),
        "/file/.".to_string(),
        "/file/../z".to_string(),
        "/have/file/x".to_string(),
        "/filelink".to_string(),
        "/filelink/".to_string(),
        "/filelink/x".to_string(),
        "/dangle".to_string(),
        "/dangle/".to_string(),
        format!("/{long}"),
        format!("/{long}/x"),
        format!("/have/{long}"),
        format!("/file/{long}"),
        "/sub/file".to_string(),
        "/sub/file/x".to_string(),
        format!("/sub/{long}"),
        String::new(),
    ];
    let mut script = String::from(
        "cd \"$MNT\"\n\
         mkdir -p have/deep\n\
         printf x > file\n\
         printf y > have/file\n\
         ln -s file filelink\n\
         ln -s nowhere dangle\n\
         btrfs subvolume create sub >/dev/null\n\
         printf z > sub/file\n\
         sync\n\
         refuse() {\n\
         \x20   if out=\"$(LC_ALL=C mkdir -p -- \"$1\" 2>&1)\"; then\n\
         \x20       printf '%s|NOT REFUSED\\n' \"$1\"\n\
         \x20   else\n\
         \x20       printf '%s|%s\\n' \"$1\" \"${out##*: }\"\n\
         \x20   fi\n\
         }\n",
    );
    for path in &cases {
        let relative = path.strip_prefix('/').unwrap_or(path);
        script.push_str(&format!("refuse '{relative}'\n"));
    }
    let out = guest_kernel_write(&img, &script);
    assert!(
        out.status.success(),
        "the kernel could not fill the volume or try the refusals:\n{}{}",
        stdout(&out),
        stderr(&out)
    );
    let kernel = stdout(&out);
    let image = std::fs::read(&img).expect("read the image");

    let mut tried = Vec::new();
    for line in kernel.lines() {
        let (relative, reason) = line
            .split_once('|')
            .unwrap_or_else(|| panic!("a guest refusal line: {line}\nall of them:\n{kernel}"));
        let want = ours(reason);
        let path = if relative.is_empty() {
            String::new()
        } else {
            format!("/{relative}")
        };
        let got = tool("fs.btrfs")
            .args([img.as_str(), "mkdir", "-p", path.as_str()])
            .output()
            .expect("spawn fs.btrfs");
        let err = stderr(&got);
        let label = &path[..path.len().min(40)];
        let what = format!(
            "mkdir -p {label:?}: the kernel said {reason:?}; fs.btrfs said {err}\n\
             the kernel's answers:\n{kernel}"
        );
        assert_eq!(got.status.code(), Some(1), "{what}");
        assert!(got.stdout.is_empty(), "{what}; stdout: {}", stdout(&got));
        assert!(err.contains("\"code\": 1"), "{what}");
        assert!(err.contains(want), "{what}; expected {want:?}");
        assert!(
            std::fs::read(&img).expect("read the image") == image,
            "mkdir -p {label:?} was refused and still changed the image"
        );
        tried.push(path);
    }
    assert_eq!(tried, cases, "the guest answered every case:\n{kernel}");
}
