//! TEMPORARY exploration for #177; removed before merge.
//!   cargo run --example explore177 -- IMAGE DIRTY_ADDR
use fs_btrfs::fs::Filesystem;
use fs_btrfs::super_write::Commit;
use fs_core::{BlockDevice, FileDevice};
use std::sync::Arc;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let image = &args[1];
    let dirty: u64 = args[2].parse().unwrap();
    let dev = Arc::new(FileDevice::open_rw(image).unwrap());
    let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount rw");
    let groups = fs.block_groups().unwrap();
    let generation = fs.superblock().generation + 1;
    let plan = match fs.plan_transaction_closed(&[dirty], 64) {
        Ok(p) => p,
        Err(e) => {
            println!("PLAN-REFUSED {e}");
            std::process::exit(3);
        }
    };
    for r in &plan.rewrites {
        let g = |a: u64| groups.iter().find(|g| g.contains(a)).map(|g| g.start);
        println!(
            "rewrite owner {} old {} (group {:?}) -> new {} (group {:?})",
            r.owner,
            r.old,
            g(r.old),
            r.new,
            g(r.new)
        );
    }
    let blocks = match fs.render_plan(&plan, generation) {
        Ok(b) => b,
        Err(e) => {
            println!("RENDER-REFUSED {e}");
            std::process::exit(3);
        }
    };
    let root = fs.planned_root(&plan).expect("root moves");
    match fs.commit(
        &blocks,
        &Commit {
            generation,
            root,
            invalidate_free_space_tree: false,
            ..Default::default()
        },
    ) {
        Ok(()) => println!("COMMITTED"),
        Err(e) => {
            println!("COMMIT-REFUSED {e}");
            std::process::exit(3);
        }
    }
}
