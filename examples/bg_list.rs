//! Which block groups the extent tree reports.

// The read-only openers deprecated in 0.12.0 stay in use here for their
// deprecation release (`layers::BtrfsVolume` replaces them).
#![allow(deprecated)]
use fs_btrfs::fs::Filesystem;
use fs_core::FileDevice;
use std::sync::Arc;
fn main() {
    for path in std::env::args().skip(1) {
        let Ok(dev) = FileDevice::open(&path) else {
            continue;
        };
        let Ok(fs) = Filesystem::mount(Arc::new(dev)) else {
            continue;
        };
        println!("{path}");
        for g in fs.block_groups().unwrap_or_default() {
            println!(
                "  start {:<12} len {:<10} used {:<10} flags {:#06x}",
                g.start, g.length, g.used, g.flags
            );
        }
    }
}
