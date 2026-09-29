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
    for grp in groups.iter().filter(|x| {
        plan.released()
            .iter()
            .chain(plan.allocated().iter())
            .any(|a| x.contains(*a))
    }) {
        let ours = fs.free_extents(grp).unwrap();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for r in &ours {
            match merged.last_mut() {
                Some(p) if p.0 + p.1 == r.start => p.1 += r.len,
                _ => merged.push((r.start, r.len)),
            }
        }
        let ns = fs.superblock().nodesize as u64;
        let mut sim: Vec<(u64, u64)> = ours.iter().map(|r| (r.start, r.len)).collect();
        let rel_here: Vec<u64> = plan
            .released()
            .into_iter()
            .filter(|a| grp.contains(*a))
            .collect();
        let alloc_here: Vec<u64> = plan
            .allocated()
            .into_iter()
            .filter(|a| grp.contains(*a))
            .collect();
        sim.extend(rel_here.iter().map(|a| (*a, ns)));
        sim.sort();
        let mut m2: Vec<(u64, u64)> = Vec::new();
        for r in sim {
            match m2.last_mut() {
                Some(p) if p.0 + p.1 == r.0 => p.1 += r.1,
                _ => m2.push(r),
            }
        }
        for a in &alloc_here {
            m2 = m2
                .into_iter()
                .flat_map(|(s0, l0)| {
                    let e0 = s0 + l0;
                    if a + ns <= s0 || *a >= e0 {
                        vec![(s0, l0)]
                    } else {
                        let mut v = vec![];
                        if *a > s0 {
                            v.push((s0, a - s0));
                        }
                        if a + ns < e0 {
                            v.push((a + ns, e0 - a - ns));
                        }
                        v
                    }
                })
                .collect();
        }
        println!(
            "   released here {} allocated here {} -> runs after the transaction {}",
            rel_here.len(),
            alloc_here.len(),
            m2.len()
        );
        let cached = fs.cached_free_extents(grp).unwrap();
        println!(
            "touched group {} flags {:#x}: extent-tree gaps {} merged {} ; fst {:?}",
            grp.start,
            grp.flags,
            ours.len(),
            merged.len(),
            cached.as_ref().map(|c| c.len())
        );
        if let Some(c) = cached {
            let firstdiff = merged
                .iter()
                .zip(c.iter())
                .position(|(a, b)| a.0 != b.start || a.1 != b.len);
            if let Some(i) = firstdiff {
                println!("   first diff at {i}: ours {:?} fst {:?}", merged[i], c[i]);
            }
        }
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
