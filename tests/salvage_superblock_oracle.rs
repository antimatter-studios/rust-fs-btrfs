//! Salvage reads every superblock copy, and the backup ring each holds,
//! as btrfs-progs reads them.
//!
//! The oracle is `btrfs inspect-internal dump-super -f -s N` in the harness
//! VM, run on kernel-made fixtures and on copies of them with one copy
//! damaged. For every copy it says whether the checksum matches, what
//! generation and roots the copy names, and what each backup slot holds;
//! `salvage::superblock_copies` has to agree on all of it, including for a
//! copy whose checksum fails, because those fields are where an
//! older-generation walk starts.

use fs_btrfs::salvage::{best_copy, superblock_copies, CopyVerdict, SuperblockCopy};
use fs_btrfs::super_write::BackupRoots;
use fs_btrfs_test_support::{copy_sparse, fixture, fixtures_matching, oracle, temp_path};
use fs_core::FileDevice;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

const MIRRORS: [u64; 3] = [0x1_0000, 0x400_0000, 0x40_0000_0000];

/// What `dump-super` printed for one copy.
#[derive(Debug, Default)]
struct Dump {
    /// The tool refused the copy for its magic.
    bad_magic: bool,
    /// It printed nothing: the copy lies past the end of the image.
    empty: bool,
    /// `csum ... [match]` or `[DON'T MATCH]`.
    csum_match: Option<bool>,
    fields: BTreeMap<String, u64>,
    /// `backup N` -> field -> the numbers on its line.
    backups: BTreeMap<usize, BTreeMap<String, Vec<u64>>>,
}

fn numbers(text: &str) -> Vec<u64> {
    text.split_whitespace()
        .filter_map(|word| word.parse().ok())
        .collect()
}

fn dump_super(image: &Path, mirror: usize, force: bool) -> Dump {
    let mut tool = oracle("btrfs").args(["inspect-internal", "dump-super", "-f"]);
    if force {
        tool = tool.arg("-F");
    }
    let out = tool.args(["-s", &mirror.to_string()]).arg(image).output();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut dump = Dump {
        bad_magic: stderr.contains("bad magic"),
        empty: stdout.trim().is_empty(),
        ..Dump::default()
    };
    assert!(
        out.status.success() || dump.bad_magic,
        "dump-super -s {mirror} {}: {stderr}",
        image.display()
    );
    let mut slot = None;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("backup ") {
            slot = rest.trim_end_matches(':').parse().ok();
            continue;
        }
        let Some((key, value)) = trimmed.split_once(['\t', ':']) else {
            continue;
        };
        let key = key.trim_end_matches(':').to_string();
        let value = value.trim_start_matches(':').trim();
        if key == "csum" {
            dump.csum_match = Some(value.ends_with("[match]"));
        }
        match slot {
            Some(n) if line.starts_with("\t\t") => {
                dump.backups
                    .entry(n)
                    .or_default()
                    .insert(key, numbers(value));
            }
            _ => {
                if let Some(&n) = numbers(value).first() {
                    dump.fields.entry(key).or_insert(n);
                }
            }
        }
    }
    dump
}

fn triple(root: &fs_btrfs::super_write::BackupRoot) -> Vec<u64> {
    vec![root.bytenr, root.generation, u64::from(root.level)]
}

/// Every field of a backup slot, keyed as `dump-super -f` prints them.
fn backup_fields(backup: &BackupRoots) -> BTreeMap<String, Vec<u64>> {
    [
        ("backup_tree_root", triple(&backup.tree)),
        ("backup_chunk_root", triple(&backup.chunk)),
        ("backup_extent_root", triple(&backup.extent)),
        ("backup_fs_root", triple(&backup.fs)),
        ("backup_dev_root", triple(&backup.dev)),
        ("csum_root", triple(&backup.csum)),
        ("backup_total_bytes", vec![backup.total_bytes]),
        ("backup_bytes_used", vec![backup.bytes_used]),
        ("backup_num_devices", vec![backup.num_devices]),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect()
}

/// The copy's decoded fields and backup ring agree with the dump.
fn assert_agrees(label: &str, copy: &SuperblockCopy, dump: &Dump) {
    let sb = copy
        .superblock
        .as_ref()
        .unwrap_or_else(|| panic!("{label}: copy {} was not decoded", copy.mirror));
    for (key, ours) in [
        ("generation", sb.generation),
        ("root", sb.root),
        ("chunk_root", sb.chunk_root),
        ("total_bytes", sb.total_bytes),
        ("bytes_used", sb.bytes_used),
        ("num_devices", sb.num_devices),
        ("nodesize", u64::from(sb.nodesize)),
    ] {
        assert_eq!(
            Some(&ours),
            dump.fields.get(key),
            "{label}: copy {} field {key}",
            copy.mirror
        );
    }
    assert_eq!(copy.backups.len(), 4, "{label}: the ring holds four slots");
    assert_eq!(
        dump.backups.len(),
        4,
        "{label}: dump-super printed four slots"
    );
    for (slot, backup) in copy.backups.iter().enumerate() {
        assert_eq!(
            &backup_fields(backup),
            &dump.backups[&slot],
            "{label}: copy {} backup slot {slot}",
            copy.mirror
        );
    }
}

fn copies_of(image: &Path) -> Vec<SuperblockCopy> {
    let device = FileDevice::open(image).expect("open the image");
    superblock_copies(&device)
}

/// Every copy of every checksum type's fixture, and of the populated one,
/// reads as btrfs-progs reads it.
#[test]
fn every_copy_and_backup_ring_matches_dump_super() {
    let mut images = fixtures_matching("btrfs-csum-");
    images.push(fixture("btrfs-rich.img"));
    for image in &images {
        let label = image.file_name().unwrap().to_string_lossy().into_owned();
        let copies = copies_of(image);
        assert_eq!(copies.len(), 3, "{label}");
        for copy in &copies {
            let dump = dump_super(image, copy.mirror, false);
            assert_eq!(copy.offset, MIRRORS[copy.mirror]);
            if dump.empty {
                assert_eq!(
                    copy.verdict,
                    CopyVerdict::Absent,
                    "{label}: copy {}",
                    copy.mirror
                );
                continue;
            }
            assert_eq!(dump.csum_match, Some(true), "{label}: copy {}", copy.mirror);
            assert_eq!(
                copy.verdict,
                CopyVerdict::Valid,
                "{label}: copy {}",
                copy.mirror
            );
            assert_agrees(&label, copy, &dump);
        }
        let best = best_copy(&copies).expect("an undamaged fixture has a valid copy");
        assert!(
            copies
                .iter()
                .filter(|c| c.verdict == CopyVerdict::Valid)
                .all(|c| c.generation() <= best.generation()),
            "{label}: the best copy is the newest valid one"
        );
        assert!(
            !best.backups_newest_first().is_empty(),
            "{label}: a kernel-made volume has filled backup slots"
        );
    }
}

/// A copy of `btrfs-rich.img` with `damage` applied, at a path the guest
/// sees.
fn damaged(tag: &str, damage: &[(u64, &[u8])]) -> PathBuf {
    let dir = PathBuf::from(temp_path!("salvage-superblock-{tag}"));
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    let image = dir.join("damaged.img");
    copy_sparse(fixture("btrfs-rich.img"), &image).expect("copy the fixture");
    let file = OpenOptions::new()
        .write(true)
        .open(&image)
        .expect("open the copy");
    for (offset, bytes) in damage {
        file.write_all_at(bytes, *offset).expect("damage the copy");
    }
    image
}

/// The primary's label byte changed: the checksum fails, the fields still
/// decode, and the mirror at 64 MiB is the copy a mount uses.
#[test]
fn a_primary_whose_checksum_fails_still_decodes_and_the_mirror_wins() {
    let image = damaged("primary-csum", &[(MIRRORS[0] + 0x12b, b"X")]);
    let copies = copies_of(&image);

    let primary = dump_super(&image, 0, false);
    assert_eq!(
        primary.csum_match,
        Some(false),
        "btrfs-progs sees the damage"
    );
    assert_eq!(copies[0].verdict, CopyVerdict::ChecksumMismatch);
    assert_agrees("primary", &copies[0], &primary);

    let mirror = dump_super(&image, 1, false);
    assert_eq!(mirror.csum_match, Some(true));
    assert_eq!(copies[1].verdict, CopyVerdict::Valid);
    assert_eq!(best_copy(&copies).map(|c| c.mirror), Some(1));
}

/// The mirror's magic zeroed: btrfs-progs refuses it for its magic, and
/// salvage calls it not Btrfs and decodes nothing from it.
#[test]
fn a_copy_without_its_magic_is_not_btrfs() {
    let image = damaged("mirror-magic", &[(MIRRORS[1] + 0x40, &[0u8; 8])]);
    let copies = copies_of(&image);

    let dump = dump_super(&image, 1, false);
    assert!(dump.bad_magic, "btrfs-progs refuses the copy for its magic");
    assert_eq!(copies[1].verdict, CopyVerdict::NotBtrfs);
    assert_eq!(copies[1].superblock, None);
    assert!(copies[1].backups.is_empty());

    assert_eq!(copies[0].verdict, CopyVerdict::Valid);
    assert_eq!(best_copy(&copies).map(|c| c.mirror), Some(0));
}

/// Both copies damaged: no copy is valid, and the damaged primary's
/// backup ring is still there to start from.
#[test]
fn with_no_valid_copy_the_backup_ring_is_still_read() {
    let image = damaged(
        "both",
        &[(MIRRORS[0] + 0x12b, b"X"), (MIRRORS[1] + 0x12b, b"X")],
    );
    let copies = copies_of(&image);
    assert_eq!(best_copy(&copies), None);
    for copy in &copies[..2] {
        let dump = dump_super(&image, copy.mirror, false);
        assert_eq!(dump.csum_match, Some(false));
        assert_eq!(copy.verdict, CopyVerdict::ChecksumMismatch);
        assert_agrees("both", copy, &dump);
    }
    assert!(!copies[0].backups_newest_first().is_empty());
}
