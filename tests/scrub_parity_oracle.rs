//! A scrub finds a damaged RAID5/6 parity element where it was damaged,
//! and the parity it recomputes is the kernel's (#301).
//!
//! The fixtures are `scrub/btrfs-scrub-raid5-*` (three devices) and
//! `scrub/btrfs-scrub-raid6-*` (four): pools the kernel populated, then
//! P damaged in one full stripe of an 8 MiB file and, on RAID6, Q in the
//! next. Where each element lies was worked out in the guest from the
//! chunk tree as btrfs-progs prints it and the kernel's rotation, not by
//! this crate. A copy of each damaged pool was then scrubbed by the
//! kernel with a scrub that may write, which rewrites the parity it finds
//! wrong; the manifest holds the SHA-256 of each element as the kernel
//! rewrote it, and the addresses of the data elements it is computed
//! from.
//!
//! # What each test is a check on
//!
//! - **The damaged elements are reported, and nothing else is.** A read
//!   never looks at parity while the data verifies, so only a scrub that
//!   recomputes it can find these. Each is named by the device and byte
//!   offset the guest damaged, as P or Q.
//! - **The parity recomputed is the kernel's.** The data elements are
//!   read raw from the damaged pool (they are undamaged) and P or Q
//!   computed from them; it must hash to what the kernel wrote back.
//! - **Undamaged pools report nothing,** having checked some full
//!   stripes: the RAID5 and RAID6 pools the read tests use.

use fs_btrfs::fs::Filesystem;
use fs_btrfs::raid56;
use fs_btrfs::scrub::{ScrubReport, ScrubTarget};
use fs_btrfs_test_support::{fixture, sha256_hex};
use fs_core::{BlockRead, FileDevice};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// One damaged element, from the manifest.
#[derive(Debug)]
struct Damaged {
    /// `P` or `Q`.
    what: String,
    devid: u64,
    physical: u64,
    stripe_len: u64,
    /// The full stripe's data elements, `(devid, physical)` in data order.
    data: Vec<(u64, u64)>,
    /// SHA-256 of the element as the kernel's scrub rewrote it.
    kernel: String,
}

fn manifest(profile: &str) -> Vec<Damaged> {
    let path = fixture(&format!("scrub/btrfs-scrub-{profile}.manifest"));
    let text = std::fs::read_to_string(path).expect("scrub parity manifest");
    let mut data: BTreeMap<String, Vec<(u64, u64)>> = BTreeMap::new();
    let mut kernel: BTreeMap<String, String> = BTreeMap::new();
    let mut damaged = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        match f.first().copied() {
            Some("damaged") => damaged.push((
                f[1].to_string(),
                f[2].parse::<u64>().expect("devid"),
                f[3].parse::<u64>().expect("physical"),
                f[4].parse::<u64>().expect("stripe_len"),
            )),
            Some("data") => {
                let elements = f[2..]
                    .iter()
                    .map(|e| {
                        let (d, p) = e.split_once(':').expect("devid:physical");
                        (d.parse().expect("devid"), p.parse().expect("physical"))
                    })
                    .collect();
                data.insert(f[1].to_string(), elements);
            }
            Some("kernel") => {
                kernel.insert(f[1].to_string(), f[2].to_string());
            }
            _ => {}
        }
    }
    damaged
        .into_iter()
        .map(|(what, devid, physical, stripe_len)| Damaged {
            data: data
                .remove(&what)
                .expect("a data line for each damaged element"),
            kernel: kernel
                .remove(&what)
                .expect("a kernel line for each damaged element"),
            what,
            devid,
            physical,
            stripe_len,
        })
        .collect()
}

/// The members of a pool, in devid order, from `name-1.img` on.
fn members(name: &str, count: usize) -> Vec<FileDevice> {
    (1..=count)
        .map(|i| FileDevice::open(fixture(&format!("{name}-{i}.img"))).expect("open a member"))
        .collect()
}

fn scrub(name: &str, count: usize) -> ScrubReport {
    let devices: Vec<Arc<dyn BlockRead>> = members(name, count)
        .into_iter()
        .map(|d| Arc::new(d) as Arc<dyn BlockRead>)
        .collect();
    let fs = Filesystem::mount_pool(devices).expect("a pool with damaged parity still mounts");
    fs.scrub().expect("scrub")
}

fn damaged_elements_are_reported_and_nothing_else(profile: &str, count: usize, expect: usize) {
    let damaged = manifest(profile);
    assert_eq!(
        damaged.len(),
        expect,
        "the {profile} fixture damages {expect} elements"
    );
    let report = scrub(&format!("scrub/btrfs-scrub-{profile}"), count);
    assert!(
        report.parity_stripes > 0,
        "no full stripe was checked: {report:#?}"
    );
    let want: BTreeSet<(usize, Option<(u64, u64)>)> = damaged
        .iter()
        .map(|d| (usize::from(d.what == "Q"), Some((d.devid, d.physical))))
        .collect();
    let found: BTreeSet<(usize, Option<(u64, u64)>)> =
        report.errors.iter().map(|e| (e.mirror, e.device)).collect();
    assert_eq!(found, want, "{:#?}", report.errors);
    assert!(
        report
            .errors
            .iter()
            .all(|e| e.what == ScrubTarget::Parity && e.repairable),
        "only parity is damaged, and its data verifies: {:#?}",
        report.errors
    );
}

#[test]
fn raid5_damaged_p_is_reported_where_it_was_damaged() {
    damaged_elements_are_reported_and_nothing_else("raid5", 3, 1);
}

#[test]
fn raid6_damaged_p_and_q_are_reported_where_they_were_damaged() {
    damaged_elements_are_reported_and_nothing_else("raid6", 4, 2);
}

/// P or Q computed from the data elements read raw, against the hash of
/// the element the kernel wrote back.
#[test]
fn the_parity_recomputed_is_the_kernels() {
    for (profile, count) in [("raid5", 3), ("raid6", 4)] {
        let devices = members(&format!("scrub/btrfs-scrub-{profile}"), count);
        for d in manifest(profile) {
            let data: Vec<Vec<u8>> = d
                .data
                .iter()
                .map(|&(devid, physical)| {
                    let mut buf = vec![0u8; d.stripe_len as usize];
                    devices[(devid - 1) as usize]
                        .read_at(physical, &mut buf)
                        .expect("read a data element");
                    buf
                })
                .collect();
            let refs: Vec<&[u8]> = data.iter().map(Vec::as_slice).collect();
            let (p, q) = raid56::parity(&refs, profile == "raid6");
            let ours = if d.what == "P" {
                p
            } else {
                q.expect("Q on RAID6")
            };
            assert_eq!(
                sha256_hex(&ours),
                d.kernel,
                "{profile} {} at device {}, {}: the parity computed here is not the kernel's",
                d.what,
                d.devid,
                d.physical
            );
        }
    }
}

#[test]
fn undamaged_parity_pools_report_nothing() {
    for (name, count) in [("btrfs-raid5", 3), ("btrfs-raid6", 4)] {
        let report = scrub(name, count);
        assert!(
            report.parity_stripes > 0,
            "{name}: no full stripe was checked"
        );
        assert!(report.errors.is_empty(), "{name}: {:#?}", report.errors);
    }
}
