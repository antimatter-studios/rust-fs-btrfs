//! GNU find over the guest kernel's own mount is the oracle, including
//! nested subvolumes and a snapshot stored inside its own ancestor.
mod cli_support;

use cli_support::*;
use fs_btrfs_test_support::{guest_kernel_write, oracle};
use std::collections::BTreeMap;

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

#[test]
fn recursive_ls_matches_find_across_subvolumes_and_an_ancestor_snapshot() {
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
    let report = stdout(&ok(tool("fs.btrfs").args([img, "ls", "--recursive", "/"])));
    let listed = rows(&report);
    let mut actual = BTreeMap::new();
    for (path, row) in &listed {
        assert_eq!(json_field(row, "depth"), components(path).len().to_string());
        assert!(
            !row.contains("\"revisited\": true"),
            "{}",
            String::from_utf8_lossy(path)
        );
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
    assert_eq!(
        actual, expected,
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
}
