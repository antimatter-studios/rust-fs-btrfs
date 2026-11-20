//! The three layers read what the Linux kernel reads.
//!
//! The kernel mounts each fixture read-only in the harness VM and reports
//! every path with its type, mode, size, SHA-256 and link target. The file
//! layer has to list the same paths, subvolumes included, with the same
//! values. The salvage verbs have to rebuild the same files without the
//! file layer: candidates found by scanning, names from the rebuilt tree,
//! bytes from each walk's materialized plan.

use fs_btrfs::layers::{BtrfsDetail, BtrfsVolume};
use fs_btrfs_test_support::{fixture, guest_kernel_report, sha256_hex};
use fs_core::layers::{
    Candidate, CandidateKind, CandidateState, FileAPI, NodeKind, OpenVolume, Parent, PrimitiveAPI,
    WalkEnd,
};
use fs_core::FileDevice;
use std::collections::{BTreeMap, HashMap};

type Report = BTreeMap<(String, String), String>;

const FIXTURES: &[&str] = &[
    "btrfs-rich.img",
    "btrfs-comp-zlib.img",
    "btrfs-comp-lzo.img",
    "btrfs-comp-zstd.img",
    "btrfs-subvol.img",
    "btrfs-nodatacow.img",
];

/// What the file layer lists, in the kernel report's terms.
fn through_files(volume: &BtrfsVolume<FileDevice>) -> Report {
    let mut report = Report::new();
    let mut stack = vec![(volume.root().unwrap(), String::new())];
    while let Some((directory, prefix)) = stack.pop() {
        for entry in volume.read_dir(&directory).unwrap() {
            let path = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{prefix}/{}", entry.name)
            };
            let metadata = volume.metadata(&entry.node).unwrap();
            let mode = format!("{:o}", metadata.mode.unwrap());
            report.insert(("mode".into(), path.clone()), mode);
            match entry.kind {
                NodeKind::Directory => {
                    report.insert(("type".into(), path.clone()), "directory".into());
                    stack.push((entry.node, path));
                }
                NodeKind::File => {
                    let kind = if metadata.size == 0 {
                        "regular-empty-file"
                    } else {
                        "regular-file"
                    };
                    report.insert(("type".into(), path.clone()), kind.into());
                    report.insert(("size".into(), path.clone()), metadata.size.to_string());
                    let mut bytes = Vec::new();
                    volume.copy_to(&entry.node, &mut bytes).unwrap();
                    report.insert(("sha256".into(), path), sha256_hex(&bytes));
                }
                NodeKind::Symlink => {
                    report.insert(("type".into(), path.clone()), "symbolic-link".into());
                    let target = volume.read_link(&entry.node).unwrap();
                    report.insert(("target".into(), path), String::from_utf8(target).unwrap());
                }
                _ => {
                    report.remove(&("mode".into(), path));
                }
            }
        }
    }
    report
}

/// The kernel's report, cut to what the file layer is compared on.
fn kernel(name: &str) -> Report {
    let image = fixture(name);
    let report = guest_kernel_report(&image.to_string_lossy(), name);
    report
        .into_iter()
        .filter(|((kind, _), _)| {
            matches!(
                kind.as_str(),
                "type" | "mode" | "size" | "sha256" | "target"
            )
        })
        .filter(|((kind, _), value)| {
            kind != "type"
                || matches!(
                    value.as_str(),
                    "directory" | "regular-file" | "regular-empty-file" | "symbolic-link"
                )
        })
        .collect()
}

/// Every path the kernel lists, the file layer lists the same.
#[test]
fn the_file_layer_lists_what_the_kernel_lists() {
    for name in FIXTURES {
        let theirs = kernel(name);
        let volume = BtrfsVolume::open(FileDevice::open(fixture(name)).unwrap()).unwrap();
        let ours = through_files(&volume);
        assert!(
            theirs.keys().any(|(k, _)| k == "sha256"),
            "{name}: the kernel lists files"
        );
        // The kernel lists special files the file layer leaves out of this
        // comparison; their mode lines go with them.
        let theirs: Report = theirs
            .iter()
            .filter(|((kind, path), _)| {
                kind != "mode" || theirs.contains_key(&("type".to_string(), path.clone()))
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        assert_eq!(ours, theirs, "{name}");
    }
}

/// The salvage verbs alone rebuild every regular file the kernel lists in
/// the top-level tree: a live candidate, named through the rebuilt tree, its
/// walk complete, its plan materializing the kernel's bytes.
#[test]
fn the_salvage_verbs_rebuild_what_the_kernel_lists() {
    for name in FIXTURES.iter().filter(|n| **n != "btrfs-subvol.img") {
        let theirs = kernel(name);
        let volume =
            BtrfsVolume::open_for_salvage(FileDevice::open(fixture(name)).unwrap()).unwrap();
        let salvage = volume.salvage().unwrap();
        let candidates: Vec<Candidate<BtrfsDetail>> = salvage
            .all_candidates()
            .unwrap()
            .into_iter()
            .filter(|c| c.state == CandidateState::Live && c.detail.tree == 5)
            .collect();
        let tree = salvage.rebuild_tree(&candidates).unwrap();
        let by_offset: HashMap<u64, usize> = candidates
            .iter()
            .enumerate()
            .map(|(i, c)| (c.location.offset, i))
            .collect();
        // A directory's path through its first claim; a file is named by
        // each of its claims, one per hard link.
        let path_of = |mut index: usize, mut parts: Vec<String>| -> Option<String> {
            for _ in 0..64 {
                let node = &tree.nodes[index];
                let claim = node.parents.first()?;
                match claim.parent {
                    Parent::Root => {
                        parts.reverse();
                        return Some(parts.join("/"));
                    }
                    Parent::Candidate(at) => {
                        parts.push(String::from_utf8(claim.name.clone()?).ok()?);
                        index = *by_offset.get(&at)?;
                    }
                }
            }
            None
        };
        let mut rebuilt = 0;
        for (index, candidate) in candidates.iter().enumerate() {
            if candidate.kind != CandidateKind::File {
                continue;
            }
            let walk = salvage.walk_extents(candidate).unwrap();
            assert_eq!(walk.end, WalkEnd::Complete, "{name} {:?}", candidate.name);
            let mut bytes = Vec::new();
            let report = salvage.materialize(&walk.plan(), &mut bytes).unwrap();
            assert_eq!(
                report.unreadable().count(),
                0,
                "{name} {:?}",
                candidate.name
            );
            let hash = sha256_hex(&bytes);
            for claim in &tree.nodes[index].parents {
                let (Parent::Candidate(at), Some(own)) = (claim.parent, claim.name.clone()) else {
                    continue;
                };
                let Some(&parent) = by_offset.get(&at) else {
                    continue;
                };
                let Ok(own) = String::from_utf8(own) else {
                    continue;
                };
                let Some(path) = path_of(parent, vec![own]) else {
                    continue;
                };
                let Some(want) = theirs.get(&("sha256".to_string(), path.clone())) else {
                    continue;
                };
                assert_eq!(&hash, want, "{name} {path}");
                rebuilt += 1;
            }
        }
        let files = theirs.keys().filter(|(kind, _)| kind == "sha256").count();
        assert_eq!(
            rebuilt, files,
            "{name}: every file the kernel lists is rebuilt"
        );
    }
}
