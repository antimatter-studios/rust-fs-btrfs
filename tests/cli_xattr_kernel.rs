//! CLI xattrs against the kernel's getfattr dump, including raw byte names.

mod cli_support;

use cli_support::*;
use fs_btrfs_test_support::{fixture, guest_kernel_read_ok, guest_kernel_write_ok};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

#[test]
fn listed_names_sizes_and_raw_values_equal_the_guest_getfattr_dump() {
    let image = fixture("btrfs-xattr.img");
    let dump = guest_kernel_read_ok(
        image.to_str().unwrap(),
        "getfattr on the kernel-made xattr fixture",
        "cd \"$MNT\"\ngetfattr -R -d -m - -e hex .\n",
    );
    let mut expected: BTreeMap<String, BTreeMap<String, Vec<u8>>> = BTreeMap::new();
    let mut current = None;
    for line in dump.lines() {
        if let Some(path) = line.strip_prefix("# file: ") {
            let path = format!("/{}", path.trim_start_matches("./"));
            expected.entry(path.clone()).or_default();
            current = Some(path);
        } else if !line.is_empty() && !line.starts_with('#') {
            let (name, hex) = line.split_once('=').unwrap_or((line, "0x"));
            let hex = hex.strip_prefix("0x").expect("getfattr -e hex");
            assert_eq!(hex.len() % 2, 0);
            let value = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            expected
                .get_mut(current.as_ref().expect("getfattr file header"))
                .unwrap()
                .insert(name.to_string(), value);
        }
    }
    assert!(
        expected.len() >= 3,
        "empty or truncated getfattr dump: {dump}"
    );
    let namespaces: BTreeSet<_> = expected
        .values()
        .flat_map(|attrs| attrs.keys().map(|name| name.split('.').next().unwrap()))
        .collect();
    assert!(namespaces.contains("user") && namespaces.contains("trusted"));
    for (path, attrs) in expected {
        let listed = stdout(&ok(tool("fs.btrfs").arg(&image).args(["xattr", &path])));
        let got: BTreeMap<_, _> = listed
            .split('{')
            .skip(1)
            .map(|object| {
                (
                    json_field(object, "name"),
                    json_field(object, "size").parse::<usize>().unwrap(),
                )
            })
            .collect();
        let sizes = attrs.iter().map(|(n, v)| (n.clone(), v.len())).collect();
        assert_eq!(got, sizes, "{path}: {listed}");
        for (name, value) in attrs {
            let out = ok(tool("fs.btrfs")
                .arg(&image)
                .args(["xattr", &path, &name, "--text"]));
            assert_eq!(out.stdout, value, "{path}: {name}");
            assert!(out.stderr.is_empty());
        }
    }
}

#[test]
fn byte_names_and_attributes_inside_subvolumes_and_snapshots_are_preserved() {
    let image = scratch_dir("xattr-byte-names").join("fs.img");
    std::fs::copy(fixture("btrfs-xattr.img"), &image).unwrap();
    guest_kernel_write_ok(
        image.to_str().unwrap(),
        "kernel attributes with byte names and subvolume paths",
        r#"name="$(printf 'user.caf\351\377')"
setfattr -n "$name" -v 0x0001ff0a00 "$MNT/bare.txt"
btrfs subvolume create "$MNT/inner"
touch "$MNT/inner/f"
setfattr -n user.comment -v 0x0001ff0a00 "$MNT/inner/f"
btrfs subvolume snapshot "$MNT/inner" "$MNT/snap"
sync
"#,
    );
    let listed = stdout(&ok(tool("fs.btrfs")
        .arg(&image)
        .args(["xattr", "/bare.txt"])));
    assert_eq!(json_field(&listed, "name"), "user.caf\u{fffd}\u{fffd}");
    assert_eq!(json_field(&listed, "name_hex"), "757365722e636166e9ff");
    assert_eq!(json_field(&listed, "size"), "5");
    let out = ok(tool("fs.btrfs")
        .arg(&image)
        .args(["xattr", "/bare.txt"])
        .arg(OsStr::from_bytes(b"user.caf\xe9\xff")));
    assert_eq!(out.stdout, [0, 1, 255, 10, 0]);
    for path in ["/inner/f", "/snap/f"] {
        let out = ok(tool("fs.btrfs")
            .arg(&image)
            .args(["xattr", path, "user.comment"]));
        assert_eq!(out.stdout, [0, 1, 255, 10, 0], "{path}");
    }
}
