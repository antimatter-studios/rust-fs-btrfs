//! `fs.btrfs subvolume create|snapshot|delete|set-read-only|set-default`
//! (#267): the
//! library's own subvolume operations reached from the command line, on
//! this crate's own `mkfs.btrfs` volume. Each change is read back through
//! the library's listing and through `ls`, and each refusal leaves the
//! image byte-identical. What the kernel makes of a subvolume the library
//! creates is tests/capi_subvol_create_kernel.rs.
//!
//! No fixture, no VM: the unit tier.

mod cli_support;

use cli_support::*;
use fs_btrfs::Filesystem;
use fs_core::{BlockDevice, FileDevice};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A fresh 128 MiB volume made by this crate's `mkfs.btrfs`.
fn volume(tag: &str) -> PathBuf {
    let img = scratch_dir(tag).join("subvol.img");
    ok(tool("mkfs.btrfs")
        .args(["-q", "--text", "--size", "128M"])
        .arg(&img));
    img
}

fn mounted(img: &Path) -> Filesystem {
    let dev = Arc::new(FileDevice::open(img).unwrap());
    Filesystem::mount(dev as Arc<dyn BlockDevice>).unwrap()
}

fn fs_btrfs(img: &Path) -> std::process::Command {
    let mut cmd = tool("fs.btrfs");
    cmd.arg(img);
    cmd
}

#[test]
fn subvolume_help_and_man_pages_name_every_verb() {
    let help = stdout(&ok(tool("fs.btrfs").args([
        "image.img",
        "subvolume",
        "--help",
    ])));
    for verb in ["create", "snapshot", "delete", "set-read-only", "set-default"] {
        assert!(help.contains(verb), "subvolume --help omits {verb}: {help}");
    }
    let create = stdout(&ok(tool("fs.btrfs").args([
        "image.img",
        "subvolume",
        "create",
        "--help",
    ])));
    for word in ["PATH", "--mode", "Examples:"] {
        assert!(
            create.contains(word),
            "create --help omits {word}: {create}"
        );
    }
    let share = scratch_dir("subvolume-docs");
    ok(entry().args(["generate", "man"]).arg(&share));
    for page in [
        "fs.btrfs-subvolume.1",
        "fs.btrfs-subvolume-create.1",
        "fs.btrfs-subvolume-snapshot.1",
        "fs.btrfs-subvolume-delete.1",
        "fs.btrfs-subvolume-set-read-only.1",
        "fs.btrfs-subvolume-set-default.1",
    ] {
        assert!(share.join("man/man1").join(page).is_file(), "no {page}");
    }
}

#[test]
fn create_reports_the_new_id_and_ls_and_the_library_show_the_subvolume() {
    let img = volume("subvolume-create");
    let out = ok(fs_btrfs(&img).args(["subvolume", "create", "/sv"]));
    let report = stdout(&out);
    assert_eq!(json_field(&report, "path"), "/sv", "{report}");
    assert_eq!(json_field(&report, "created"), "true", "{report}");
    assert_eq!(json_field(&report, "id"), "256", "{report}");

    ok(fs_btrfs(&img).args(["mkdir", "/d"]));
    let out = ok(fs_btrfs(&img).args(["subvolume", "create", "--mode", "700", "/d/inner"]));
    assert_eq!(json_field(&stdout(&out), "id"), "257");

    let listing = stdout(&ok(fs_btrfs(&img).args(["ls", "/"])));
    let sv = listing
        .split('}')
        .find(|e| e.contains("\"name\": \"sv\""))
        .unwrap_or_else(|| panic!("ls / does not list sv: {listing}"));
    assert_eq!(json_field(sv, "subvolume"), "true", "{listing}");
    let inner = stdout(&ok(fs_btrfs(&img).args(["stat", "/d/inner"])));
    assert_eq!(json_field(&inner, "mode"), "0700", "{inner}");

    let subvolumes = mounted(&img).subvolumes().unwrap();
    let found: Vec<(u64, &str, bool)> = subvolumes
        .iter()
        .map(|s| (s.id, s.path.as_str(), s.read_only))
        .collect();
    assert!(found.contains(&(256, "sv", false)), "{found:?}");
    assert!(found.contains(&(257, "d/inner", false)), "{found:?}");

    let text = stdout(&ok(fs_btrfs(&img).args([
        "--text",
        "subvolume",
        "create",
        "/third",
    ])));
    assert!(text.contains("/third") && text.contains("258"), "{text}");
}

#[test]
fn set_read_only_and_set_default_are_seen_by_the_library() {
    let img = volume("subvolume-flags");
    ok(fs_btrfs(&img).args(["subvolume", "create", "/sv"]));

    let report = stdout(&ok(fs_btrfs(&img).args([
        "subvolume",
        "set-read-only",
        "256",
        "true",
    ])));
    assert_eq!(json_field(&report, "id"), "256", "{report}");
    assert_eq!(json_field(&report, "read_only"), "true", "{report}");
    let read_only = |img: &Path| {
        mounted(img)
            .subvolumes()
            .unwrap()
            .into_iter()
            .find(|s| s.id == 256)
            .unwrap()
            .read_only
    };
    assert!(read_only(&img));
    ok(fs_btrfs(&img).args(["subvolume", "set-read-only", "256", "false"]));
    assert!(!read_only(&img));

    let report = stdout(&ok(fs_btrfs(&img).args([
        "subvolume",
        "set-default",
        "256",
    ])));
    assert_eq!(json_field(&report, "default"), "256", "{report}");
    assert_eq!(mounted(&img).default_subvolume().unwrap(), 256);
    ok(fs_btrfs(&img).args(["subvolume", "set-default", "5"]));
    assert_eq!(mounted(&img).default_subvolume().unwrap(), 5);
}

#[test]
fn refusals_name_their_reason_and_leave_the_image_as_it_was() {
    let img = volume("subvolume-refusals");
    ok(fs_btrfs(&img).args(["subvolume", "create", "/sv"]));
    ok(fs_btrfs(&img).args(["create", "/file"]));
    let before = std::fs::read(&img).unwrap();
    let unchanged = |what: &str| {
        assert!(
            std::fs::read(&img).unwrap() == before,
            "{what} changed the image"
        );
    };

    // The volume's state refuses: a name already there, a missing parent,
    // a parent that is not a directory, an id no subvolume has.
    let message = refused(fs_btrfs(&img).args(["subvolume", "create", "/sv"]), 4);
    assert!(message.contains("already exists"), "{message}");
    refused(fs_btrfs(&img).args(["subvolume", "create", "/file"]), 4);
    unchanged("an existing name");
    refused(
        fs_btrfs(&img).args(["subvolume", "create", "/missing/sv"]),
        4,
    );
    refused(fs_btrfs(&img).args(["subvolume", "create", "/file/sv"]), 4);
    unchanged("a missing or non-directory parent");
    let message = refused(fs_btrfs(&img).args(["subvolume", "set-default", "999"]), 4);
    assert!(message.contains("999"), "{message}");
    let message = refused(
        fs_btrfs(&img).args(["subvolume", "set-read-only", "999", "true"]),
        4,
    );
    assert!(message.contains("999"), "{message}");
    unchanged("an unknown id");

    // The command line is wrong: not an id, not a boolean, not octal.
    refused(fs_btrfs(&img).args(["subvolume", "set-default", "sv"]), 2);
    refused(
        fs_btrfs(&img).args(["subvolume", "set-read-only", "256", "yes"]),
        2,
    );
    refused(
        fs_btrfs(&img).args(["subvolume", "create", "--mode", "9", "/new"]),
        2,
    );
    unchanged("a wrong command line");

    // A read-only top-level subvolume: the library refuses, exit 3.
    ok(fs_btrfs(&img).args(["subvolume", "set-read-only", "5", "true"]));
    let before = std::fs::read(&img).unwrap();
    let message = refused(fs_btrfs(&img).args(["subvolume", "create", "/new"]), 3);
    assert!(message.contains("/new"), "{message}");
    assert!(
        std::fs::read(&img).unwrap() == before,
        "a read-only top level changed the image"
    );
}

#[test]
fn delete_removes_the_subvolume_and_ls_no_longer_lists_it() {
    let img = volume("subvolume-delete");
    ok(fs_btrfs(&img).args(["subvolume", "create", "/keep"]));
    ok(fs_btrfs(&img).args(["subvolume", "create", "/gone"]));
    ok(fs_btrfs(&img).args(["mkdir", "/d"]));
    ok(fs_btrfs(&img).args(["subvolume", "create", "/d/inner"]));
    ok(fs_btrfs(&img).args(["subvolume", "set-read-only", "258", "true"]));

    let report = stdout(&ok(fs_btrfs(&img).args(["subvolume", "delete", "/gone"])));
    assert_eq!(json_field(&report, "path"), "/gone", "{report}");
    assert_eq!(json_field(&report, "deleted"), "true", "{report}");
    assert_eq!(json_field(&report, "id"), "257", "{report}");
    // A read-only subvolume is deleted, as the kernel deletes a read-only
    // snapshot; and one in a directory below the top.
    let text = stdout(&ok(fs_btrfs(&img).args([
        "--text",
        "subvolume",
        "delete",
        "/d/inner",
    ])));
    assert!(text.contains("/d/inner") && text.contains("258"), "{text}");

    let listing = stdout(&ok(fs_btrfs(&img).args(["ls", "/"])));
    assert!(listing.contains("\"name\": \"keep\""), "{listing}");
    assert!(!listing.contains("\"name\": \"gone\""), "{listing}");
    let inner = stdout(&ok(fs_btrfs(&img).args(["ls", "/d"])));
    assert!(!inner.contains("inner"), "{inner}");

    let ids: Vec<u64> = mounted(&img)
        .subvolumes()
        .unwrap()
        .iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(ids, vec![5, 256], "the library still lists a deleted one");
}

#[test]
fn delete_refusals_name_their_reason_and_leave_the_image_as_it_was() {
    let img = volume("subvolume-delete-refusals");
    ok(fs_btrfs(&img).args(["subvolume", "create", "/sv"]));
    ok(fs_btrfs(&img).args(["subvolume", "create", "/default"]));
    ok(fs_btrfs(&img).args(["subvolume", "set-default", "257"]));
    ok(fs_btrfs(&img).args(["mkdir", "/plain"]));
    ok(fs_btrfs(&img).args(["create", "/file"]));
    let before = std::fs::read(&img).unwrap();
    let unchanged = |what: &str| {
        assert!(
            std::fs::read(&img).unwrap() == before,
            "{what} changed the image"
        );
    };

    // The volume's state refuses: a missing name, a name that is no
    // subvolume, a missing parent, a parent that is not a directory.
    for path in ["/missing", "/plain", "/file", "/missing/sv", "/file/sv"] {
        refused(fs_btrfs(&img).args(["subvolume", "delete", path]), 4);
        unchanged(path);
    }
    // The library refuses the default subvolume, exit 3.
    let message = refused(fs_btrfs(&img).args(["subvolume", "delete", "/default"]), 3);
    assert!(message.contains("default"), "{message}");
    unchanged("the default subvolume");
    // The command line is wrong: no path.
    refused(fs_btrfs(&img).args(["subvolume", "delete"]), 2);
    unchanged("a wrong command line");

    // A read-only top-level subvolume: the library refuses, exit 3.
    ok(fs_btrfs(&img).args(["subvolume", "set-read-only", "5", "true"]));
    let before = std::fs::read(&img).unwrap();
    let message = refused(fs_btrfs(&img).args(["subvolume", "delete", "/sv"]), 3);
    assert!(message.contains("/sv"), "{message}");
    assert!(
        std::fs::read(&img).unwrap() == before,
        "a read-only top level changed the image"
    );
}

#[test]
fn snapshot_reports_its_ids_and_the_library_lists_the_snapshot() {
    let img = volume("subvolume-snapshot");
    ok(fs_btrfs(&img).args(["mkdir", "/d"]));
    ok(fs_btrfs(&img).args(["create", "/d/f"]));
    ok(fs_btrfs(&img).args(["subvolume", "create", "/sv"]));

    // The top level, writable.
    let report = stdout(&ok(fs_btrfs(&img).args(["subvolume", "snapshot", "/", "/snap"])));
    assert_eq!(json_field(&report, "path"), "/snap", "{report}");
    assert_eq!(json_field(&report, "created"), "true", "{report}");
    assert_eq!(json_field(&report, "id"), "257", "{report}");
    assert_eq!(json_field(&report, "source"), "5", "{report}");
    assert_eq!(json_field(&report, "read_only"), "false", "{report}");
    // A subvolume, read-only, into a directory below the top.
    let report = stdout(&ok(fs_btrfs(&img).args([
        "subvolume",
        "snapshot",
        "-r",
        "/sv",
        "/d/sv-ro",
    ])));
    assert_eq!(json_field(&report, "id"), "258", "{report}");
    assert_eq!(json_field(&report, "source"), "256", "{report}");
    assert_eq!(json_field(&report, "read_only"), "true", "{report}");

    // The snapshot of the top level holds what the top level held when it
    // was taken, and not its own entry.
    let file = stdout(&ok(fs_btrfs(&img).args(["stat", "/snap/d/f"])));
    assert_eq!(json_field(&file, "type"), "file", "{file}");
    let listing = stdout(&ok(fs_btrfs(&img).args(["ls", "/snap"])));
    assert!(listing.contains("\"name\": \"d\""), "{listing}");
    assert!(!listing.contains("\"name\": \"snap\""), "{listing}");

    let subvolumes = mounted(&img).subvolumes().unwrap();
    let found: Vec<(u64, &str, bool, bool)> = subvolumes
        .iter()
        .map(|s| (s.id, s.path.as_str(), s.read_only, s.is_snapshot))
        .collect();
    assert!(found.contains(&(257, "snap", false, true)), "{found:?}");
    assert!(found.contains(&(258, "d/sv-ro", true, true)), "{found:?}");

    let text = stdout(&ok(fs_btrfs(&img).args([
        "--text",
        "subvolume",
        "snapshot",
        "/sv",
        "/third",
    ])));
    assert!(
        text.contains("/third") && text.contains("259") && text.contains("256"),
        "{text}"
    );
}

#[test]
fn snapshot_refusals_name_their_reason_and_leave_the_image_as_it_was() {
    let img = volume("subvolume-snapshot-refusals");
    ok(fs_btrfs(&img).args(["subvolume", "create", "/sv"]));
    ok(fs_btrfs(&img).args(["mkdir", "/plain"]));
    ok(fs_btrfs(&img).args(["create", "/file"]));
    let before = std::fs::read(&img).unwrap();
    let unchanged = |what: &str| {
        assert!(
            std::fs::read(&img).unwrap() == before,
            "{what} changed the image"
        );
    };

    // The volume's state refuses: a source that is no subvolume's top
    // directory, a destination already there, a missing parent, a parent
    // that is not a directory.
    for source in ["/missing", "/plain", "/file"] {
        refused(
            fs_btrfs(&img).args(["subvolume", "snapshot", source, "/new"]),
            4,
        );
        unchanged(source);
    }
    let message = refused(
        fs_btrfs(&img).args(["subvolume", "snapshot", "/sv", "/plain"]),
        4,
    );
    assert!(message.contains("already exists"), "{message}");
    for dest in ["/sv", "/missing/new", "/file/new"] {
        refused(
            fs_btrfs(&img).args(["subvolume", "snapshot", "/sv", dest]),
            4,
        );
        unchanged(dest);
    }
    // The command line is wrong: no destination.
    refused(fs_btrfs(&img).args(["subvolume", "snapshot", "/sv"]), 2);
    unchanged("a wrong command line");

    // A read-only top-level subvolume: the library refuses, exit 3.
    ok(fs_btrfs(&img).args(["subvolume", "set-read-only", "5", "true"]));
    let before = std::fs::read(&img).unwrap();
    let message = refused(
        fs_btrfs(&img).args(["subvolume", "snapshot", "/sv", "/new"]),
        3,
    );
    assert!(message.contains("/new"), "{message}");
    assert!(
        std::fs::read(&img).unwrap() == before,
        "a read-only top level changed the image"
    );
}
