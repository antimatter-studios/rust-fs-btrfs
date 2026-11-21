//! rust-fs-core's salvage conformance set (`conformance::salvage::run`)
//! over a small kernel-written volume and damaged copies of it.
//!
//! The set holds whole images in memory, so the volume is made small:
//! `mkfs.btrfs --mixed` makes 16 MiB (the smallest a non-mixed volume
//! can be is 109 MiB), and the kernel, in the harness VM, writes
//! plain, inline, sparse, nested and zstd-compressed files into it, with a
//! hard link and a symbolic link. The damaged copies break the primary
//! superblock's checksum, a filesystem tree leaf in every copy, and make
//! that leaf unreadable; each has to show its evidence on the
//! salvage path and be refused by the strict API.

use fs_btrfs::layers::{strict_read_all, BtrfsAdapter, LayerError};
use fs_btrfs::salvage::{
    best_copy, build_chunk_map, read_root_items, root_tree_candidates, superblock_copies,
    walk_tree, Layout,
};
use fs_btrfs_test_support::guest_kernel_write_ok;
use fs_btrfs_test_support::{oracle, read_bounded_file, temp_path};
use fs_core::layers::conformance::salvage::{run, Damaged, Driver, Evidence};
use fs_core::layers::{DamageKind, Extent};
use fs_core::BlockRead;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::OnceLock;

const MIB: u64 = 1 << 20;
/// The volume's size, and so what the set holds in memory per source: the
/// set takes whole images (rust-fs-core#453, item 4), and the test tree's
/// bounded reads stop at 16 MiB.
const IMAGE_BYTES: u64 = 16 * MIB;

/// The files the kernel writes: plain, inline, sparse, nested, compressed,
/// a hard link and a symbolic link.
const FILL: &str = r#"
mkdir -p "$MNT/sub/deeper" "$MNT/z"
btrfs property set "$MNT/z" compression zstd
printf 'small and inline\n' > "$MNT/inline.txt"
head -c 300000 /dev/urandom > "$MNT/plain.bin"
python3 -c "print('salvage conformance ' * 20000)" > "$MNT/z/compressed.txt"
printf 'compressed but short %0100d\n' 0 > "$MNT/z/short.txt"
truncate -s 1M "$MNT/sparse.bin"
printf 'tail' | dd of="$MNT/sparse.bin" bs=1 seek=900000 conv=notrunc status=none
printf 'nested\n' > "$MNT/sub/deeper/file.txt"
ln "$MNT/plain.bin" "$MNT/sub/plain-link.bin"
ln -s ../inline.txt "$MNT/sub/link"
sync
"#;

/// The healthy image, made once per test process.
fn healthy_image() -> &'static Vec<u8> {
    static IMAGE: OnceLock<Vec<u8>> = OnceLock::new();
    IMAGE.get_or_init(|| {
        let dir = PathBuf::from(temp_path!("layers-salvage-conformance"));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("small.img");
        std::fs::File::create(&image)
            .unwrap()
            .set_len(IMAGE_BYTES)
            .unwrap();
        let made = oracle("mkfs.btrfs")
            .args(["-f", "-q", "--mixed"])
            .arg(&image)
            .output();
        assert!(
            made.status.success(),
            "mkfs.btrfs: {}",
            String::from_utf8_lossy(&made.stderr)
        );
        guest_kernel_write_ok(&image.to_string_lossy(), "fill the small volume", FILL);
        read_bounded_file(&image, IMAGE_BYTES).unwrap()
    })
}

struct Memory(Vec<u8>);

impl BlockRead for Memory {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let start = offset as usize;
        buf.copy_from_slice(&self.0[start..start + buf.len()]);
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.0.len() as u64
    }
}

/// Every physical copy of the first filesystem tree leaf that holds inode
/// items, in the healthy image.
fn fs_leaf_copies() -> (Vec<u64>, u64) {
    let source = Memory(healthy_image().clone());
    let copies = superblock_copies(&source);
    let sb = best_copy(&copies).unwrap().superblock.clone().unwrap();
    let layout = Layout::new(&sb, build_chunk_map(&source, &sb).map);
    let (roots, _) = read_root_items(&source, &layout, root_tree_candidates(&copies)[0].tree_root);
    let fs = roots.iter().find(|r| r.objectid == 5).unwrap();
    let mut target = None;
    walk_tree(&source, &layout, fs.pointer(), &mut |leaf| {
        if leaf
            .items()
            .iter()
            .any(|i| i.key.key_type == 1 && i.key.objectid >= 256)
        {
            target = Some(leaf.logical);
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    let leaf = target.expect("a leaf holding inode items");
    let nodesize = u64::from(sb.nodesize);
    let physical = (0..layout.copies(leaf))
        .map(|mirror| layout.chunks.map_mirror(leaf, mirror).unwrap().physical)
        .collect();
    (physical, nodesize)
}

struct Btrfs;

impl Driver for Btrfs {
    type Adapter = BtrfsAdapter;
    type StrictError = LayerError;

    fn salvage(&self, source: &dyn BlockRead) -> BtrfsAdapter {
        BtrfsAdapter::for_source(source)
    }

    fn strict(&self, source: &dyn BlockRead) -> Result<(), LayerError> {
        strict_read_all(source)
    }

    fn healthy(&self) -> Vec<u8> {
        healthy_image().clone()
    }

    fn damaged(&self) -> Vec<Damaged> {
        let healthy = healthy_image();
        let (leaf_copies, nodesize) = fs_leaf_copies();

        let mut primary = healthy.clone();
        primary[0x1_0000 + 0x12b] ^= 0xff;

        let mut leaf = healthy.clone();
        for &at in &leaf_copies {
            let last = (at + nodesize - 1) as usize;
            leaf[last] ^= 0xff;
        }

        let mut unreadable = Damaged::new(
            "a filesystem tree leaf unreadable in every copy",
            healthy.clone(),
            Evidence::Damage(DamageKind::Unreadable),
        );
        for &at in &leaf_copies {
            unreadable = unreadable.unreadable(Extent::new(at, nodesize));
        }

        vec![
            Damaged::new(
                "the primary superblock's checksum broken",
                primary,
                Evidence::Damage(DamageKind::Checksum),
            ),
            Damaged::new(
                "a filesystem tree leaf broken in every copy",
                leaf,
                Evidence::FailedCandidate,
            ),
            unreadable,
        ]
    }
}

#[test]
fn the_salvage_adapter_conforms() {
    run(&Btrfs).assert_conforms();
}
