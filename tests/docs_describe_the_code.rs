//! The features page, the C header and the CLI's help describe the driver
//! that is here, not an older one (#272).
//!
//! Each check pairs a claim with the code that falsifies it, so a claim
//! goes stale only together with a test that says so: the header calling
//! the driver read-only beside a read-write mount it declares, the features
//! page calling copy-on-write writes planned beside the module that does them,
//! and the CLI blaming a missing copy-on-write path that exists.

use std::path::Path;

fn read(path: &str) -> String {
    let full = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("{}: {e}", full.display()))
}

fn exists(path: &str) -> bool {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(path).exists()
}

/// The features page's row that starts with `label`.
fn features_row(label: &str) -> String {
    let page = read("docs/features.md");
    page.lines()
        .find(|l| l.starts_with(&format!("| {label}")))
        .unwrap_or_else(|| panic!("docs/features.md has no row for {label:?}"))
        .to_string()
}

#[test]
fn the_header_does_not_call_a_writable_driver_read_only() {
    let header = read("include/fs_btrfs.h");
    assert!(
        header.contains("fs_btrfs_mount_rw"),
        "the header no longer declares fs_btrfs_mount_rw; this check needs rethinking"
    );
    assert!(
        !header.contains("The driver is read-only"),
        "include/fs_btrfs.h says the driver is read-only, beside the read-write mount it declares"
    );
}

#[test]
fn the_features_page_does_not_call_copy_on_write_planned_once_it_is_written() {
    assert!(
        exists("src/cow_write.rs"),
        "src/cow_write.rs has gone; this check needs rethinking"
    );
    let row = features_row("Copy-on-write writes");
    assert!(
        !row.contains("planned"),
        "docs/features.md calls copy-on-write writes planned, and src/cow_write.rs does them:\n{row}"
    );
}

#[test]
fn the_features_page_does_not_call_subvolume_crossing_missing_once_resolve_path_does_it() {
    assert!(
        read("src/subvol.rs").contains("pub fn resolve_path("),
        "resolve_path has gone; this check needs rethinking"
    );
    let row = features_row("A path that crosses into a subvolume");
    assert!(
        !row.contains("not yet"),
        "docs/features.md says no path crosses into a subvolume, and resolve_path does:\n{row}"
    );
}

#[test]
fn the_features_page_does_not_call_mirror_selection_pending_once_it_is_chosen() {
    assert!(
        read("src/superblock.rs").contains("pub fn read_superblock("),
        "read_superblock has gone; this check needs rethinking"
    );
    let row = features_row("Superblock mirrors");
    assert!(
        !row.contains("pending"),
        "docs/features.md calls mirror selection pending, and read_superblock chooses the copy:\n{row}"
    );
}

#[test]
fn the_cli_does_not_blame_a_copy_on_write_path_that_exists() {
    let cli = read("src/cli/btrfs/fs.rs");
    assert!(
        !cli.contains("no copy-on-write write path yet"),
        "src/cli/btrfs/fs.rs tells users there is no copy-on-write write path; \
         src/cow_write.rs is one"
    );
}
