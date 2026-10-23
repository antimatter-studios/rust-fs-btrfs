//! `fs.btrfs xattr` reports what THE KERNEL reports (#416): a volume made
//! by `mkfs.btrfs` (btrfs-progs) in the harness VM, its attributes set there
//! by Linux in several namespaces (user, trusted, a POSIX ACL under
//! system, and one on a symlink itself), and every path put to the guest's
//! own `getfattr -h -d -m - -e hex`. The names `xattr PATH` lists, and the
//! bytes `xattr PATH NAME` writes for each, must be the kernel's.

mod cli_support;

use cli_support::*;
use fs_btrfs_test_support::{guest_kernel_write, oracle};
use std::collections::BTreeMap;

const PATHS: &[&str] = &["/f", "/d", "/link", "/none"];

/// Fill the volume, then print each path's attributes as getfattr does,
/// each block headed `== PATH`.
const SCRIPT: &str = "cd \"$MNT\"\n\
    printf payload > f\n\
    setfattr -n user.comment -v 0x7477656c7665 f\n\
    setfattr -n user.binary -v 0x00ff800a f\n\
    setfattr -n trusted.colour -v 0x626c7565 f\n\
    setfacl -m u:1234:r f\n\
    mkdir d\n\
    setfattr -n user.dir -v 0x796573 d\n\
    ln -s f link\n\
    setfattr -h -n trusted.onlink -v 0x31 link\n\
    : > none\n\
    sync\n\
    for p in f d link none; do\n\
        echo \"== /$p\"\n\
        getfattr -h -d -m - -e hex -- \"$p\" 2>/dev/null | grep -v -e '^#' -e '^$' || true\n\
    done\n";

/// Per path, each `name=0xHEX` line the guest printed, by name.
fn kernels_view(report: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut view: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut path = None;
    for line in report.lines() {
        if let Some(p) = line.strip_prefix("== ") {
            path = Some(p.to_string());
            view.entry(p.to_string()).or_default();
            continue;
        }
        let path = path
            .as_ref()
            .unwrap_or_else(|| panic!("an attribute before any path: {line}\n{report}"));
        let (name, value) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("not name=value: {line}\n{report}"));
        let hex = value
            .strip_prefix("0x")
            .unwrap_or_else(|| panic!("not a hex value: {line}\n{report}"));
        view.get_mut(path)
            .unwrap()
            .insert(name.to_string(), hex.to_string());
    }
    view
}

/// The names in an `fs.btrfs xattr PATH` listing, as it orders them.
fn names(list: &str) -> Vec<String> {
    list.lines()
        .filter_map(|l| l.trim().strip_prefix("\"name\": \""))
        .map(|rest| rest.trim_end_matches(',').trim_end_matches('"').to_string())
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn fs_btrfs_xattr_reports_what_the_kernel_reports() {
    let image = scratch_dir("xattr-kernel").join("fs.img");
    std::fs::File::create(&image)
        .unwrap()
        .set_len(256 << 20)
        .unwrap();
    let img = image.to_str().unwrap();
    let made = oracle("mkfs.btrfs").args(["-q", "-f", img]).output();
    assert!(made.status.success(), "mkfs.btrfs: {}", stderr(&made));

    let out = guest_kernel_write(img, SCRIPT);
    assert!(
        out.status.success(),
        "the kernel could not fill the volume:\n{}{}",
        stdout(&out),
        stderr(&out)
    );
    let report = stdout(&out);
    let view = kernels_view(&report);
    assert_eq!(
        view.keys().map(String::as_str).collect::<Vec<_>>(),
        {
            let mut p = PATHS.to_vec();
            p.sort_unstable();
            p
        },
        "the guest printed a block for each path:\n{report}"
    );
    // The namespaces the test is about are all there.
    let f = &view["/f"];
    for name in [
        "user.comment",
        "user.binary",
        "trusted.colour",
        "system.posix_acl_access",
    ] {
        assert!(
            f.contains_key(name),
            "the kernel did not set {name}:\n{report}"
        );
    }
    assert!(view["/link"].contains_key("trusted.onlink"), "{report}");
    assert!(view["/none"].is_empty(), "{report}");

    for &path in PATHS {
        let list = stdout(&ok(tool("fs.btrfs").args([img, "xattr", path])));
        let kernel: Vec<String> = view[path].keys().cloned().collect();
        assert_eq!(
            names(&list),
            kernel,
            "{path}: the names, sorted\nours:\n{list}\nthe kernel's:\n{report}"
        );
        for (name, want) in &view[path] {
            let value = ok(tool("fs.btrfs").args([img, "xattr", path, name.as_str()]));
            assert_eq!(
                hex(&value.stdout),
                *want,
                "{path} {name}: the value's bytes\nthe kernel's:\n{report}"
            );
        }
    }
}
