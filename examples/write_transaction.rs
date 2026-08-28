//! Perform one transaction on a copy of a filesystem.
//!
//! Plans a relocation, renders it, and commits — the whole write path
//! end to end. The result is meant to be handed to `btrfs check` and to
//! the kernel's own mount, which is the only judgement that counts.
//!
//!   cargo run --example write_transaction -- source.img out.img

use fs_btrfs::fs::Filesystem;
use fs_btrfs::super_write::Commit;
use fs_core::{BlockDevice, FileDevice};
use std::sync::Arc;

fn main() {
    let src = std::env::args().nth(1).expect("source image");
    let out = std::env::args().nth(2).expect("output image");
    std::fs::copy(&src, &out).expect("copying the image");

    let dev = Arc::new(FileDevice::open_rw(&out).expect("opening writable"));
    let fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>).expect("mounting");

    let generation = fs.superblock().generation + 1;
    let root = fs.superblock().root;
    println!(
        "generation {} -> {generation}, root tree at {root}",
        fs.superblock().generation
    );

    // Move the root tree, and everything moving that implies.
    let plan = fs
        .plan_transaction_closed(&[root], 8)
        .expect("planning a closed transaction");
    println!(
        "plan: {} blocks, trees {:?}",
        plan.rewrites.len(),
        plan.trees()
    );
    for r in &plan.rewrites {
        println!(
            "   {} -> {}  (tree {}, level {})",
            r.old, r.new, r.owner, r.level
        );
    }

    let blocks = fs.render_plan(&plan, generation).expect("rendering");
    let new_root = fs
        .planned_root(&plan)
        .expect("the plan moves the root tree");

    fs.commit(
        &blocks,
        &Commit {
            generation,
            root: new_root,
            root_level: None,
            bytes_used: None,
            chunk_root: None,
            chunk_root_generation: None,
            // This transaction moves blocks and does not maintain the
            // free-space tree, so the cache must be marked untrusted.
            invalidate_free_space_tree: true,
        },
    )
    .expect("committing");

    println!("committed: root tree now at {new_root}");
}
