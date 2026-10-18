//! Synthetic planner shapes; the independent on-disk check is in
//! tests/node_split_kernel.rs. These do not validate Btrfs encoding.

use super::*;
use crate::btree::KeyPtr;
use crate::chunk::DiskKey;
use crate::leaf_edit::OwnedItem;
use crate::tree_write::{self, BlockIdentity, LeafItem};
use fs_core::{BlockDevice, BlockRead};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Sparse(Mutex<BTreeMap<u64, [u8; 512]>>);

impl BlockRead for Sparse {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let sectors = self.0.lock().unwrap();
        for (i, b) in buf.iter_mut().enumerate() {
            let at = offset + i as u64;
            *b = sectors.get(&(at / 512)).map_or(0, |s| s[at as usize % 512]);
        }
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        256 << 20
    }
}

impl BlockDevice for Sparse {
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_core::Result<()> {
        let mut sectors = self.0.lock().unwrap();
        for (i, b) in buf.iter().enumerate() {
            let at = offset + i as u64;
            sectors.entry(at / 512).or_insert([0; 512])[at as usize % 512] = *b;
        }
        Ok(())
    }
    fn flush(&self) -> fs_core::Result<()> {
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}

fn key(objectid: u64) -> DiskKey {
    DiskKey {
        objectid,
        key_type: 1,
        offset: 0,
    }
}

/// A full rightmost path, with distinct, readable siblings. Only tree
/// shape matters here: these synthetic items are not namespace records.
fn cascade(root_room: usize, height: u8) -> (Plan, Vec<u64>) {
    let dev = Arc::new(Sparse::default());
    crate::mkfs::format(
        dev.as_ref(),
        &crate::mkfs::Options {
            nodesize: 4096,
            ..Default::default()
        },
    )
    .unwrap();
    let fs = Filesystem::mount(dev.clone() as Arc<dyn BlockRead>).unwrap();
    let root = fs.tree_root(objectid::FS_TREE).unwrap();
    let original = fs.read_tree_block(root).unwrap();
    let id = BlockIdentity {
        bytenr: root,
        owner: objectid::FS_TREE,
        generation: fs.sb.generation,
        level: height,
        flags: tree_write::flags_for_new_block(),
        chunk_tree_uuid: tree_write::chunk_tree_uuid_of(original.bytes()),
    };
    let group = fs
        .block_groups()
        .unwrap()
        .into_iter()
        .find(|g| g.holds_metadata())
        .unwrap();
    let mut next = group.start + group.length / 2;
    let capacity = tree_write::key_ptr_capacity(&fs.sb);
    let mut number = 1;
    let mut path = Vec::new();

    struct Shape<'a> {
        fs: &'a Filesystem,
        dev: &'a Sparse,
        next: &'a mut u64,
        number: &'a mut u64,
        path: &'a mut Vec<u64>,
        capacity: usize,
        root_room: usize,
        height: u8,
    }
    impl Shape<'_> {
        fn block(&mut self, id: BlockIdentity, selected: bool) -> DiskKey {
            let first = key(*self.number);
            let bytes = if id.level == 0 {
                *self.number += 1;
                let data = vec![0; if selected { 3960 } else { 1 }];
                tree_write::build_leaf(
                    &self.fs.sb,
                    id,
                    &[LeafItem {
                        key: first,
                        data: &data,
                    }],
                )
                .unwrap()
            } else {
                let count = if !selected {
                    1
                } else if id.level == self.height {
                    self.capacity - self.root_room
                } else {
                    self.capacity
                };
                let ptrs: Vec<_> = (0..count)
                    .map(|i| {
                        let at = *self.next;
                        *self.next += 4096;
                        let child = BlockIdentity {
                            bytenr: at,
                            level: id.level - 1,
                            ..id
                        };
                        let key = self.block(child, selected && i + 1 == count);
                        KeyPtr {
                            key,
                            blockptr: at,
                            generation: id.generation,
                        }
                    })
                    .collect();
                tree_write::build_node(&self.fs.sb, id, &ptrs).unwrap()
            };
            for mirror in 0..self.fs.map.mirrors_at(id.bytenr).unwrap() {
                let physical = self.fs.map.map_mirror(id.bytenr, mirror).unwrap().physical;
                self.dev.write_at(physical, &bytes).unwrap();
            }
            if selected {
                self.path.push(id.bytenr);
            }
            first
        }
    }
    Shape {
        fs: &fs,
        dev: &dev,
        next: &mut next,
        number: &mut number,
        path: &mut path,
        capacity,
        root_room,
        height,
    }
    .block(id, true);

    // Reopen to discard cached blocks. The root address stays the same;
    // this test calls the planner directly, without committing the shape.
    drop(fs);
    let fs = Filesystem::mount(dev as Arc<dyn BlockRead>).unwrap();
    let leaf = path[0];
    let data = DataWrite {
        root: objectid::FS_TREE,
        edits: vec![ItemEdit::Put(OwnedItem {
            key: key(number),
            data: vec![1],
        })],
        ..Default::default()
    };
    let mut plan = fs.plan_transaction(&[leaf]).unwrap();
    plan.splits = fs.plan_splits(&plan, &data).unwrap();
    (plan, path)
}

#[test]
fn a_full_leaf_and_two_full_ancestors_all_split() {
    let (plan, path) = cascade(0, 2);
    for (level, old) in path.iter().enumerate() {
        let split = plan
            .split_of(*old)
            .unwrap_or_else(|| panic!("missing level-{level} split: {:?}", plan.splits));
        assert_eq!(usize::from(split.level), level);
        assert_eq!(split.root.is_some(), level == 2);
    }
    assert_eq!(plan.splits.len(), 3);
    let allocated = plan.allocated();
    assert_eq!(
        allocated.iter().copied().collect::<BTreeSet<_>>().len(),
        allocated.len()
    );
}

#[test]
fn a_cascade_stops_at_an_ancestor_with_one_pointer_free() {
    let (plan, path) = cascade(1, 3);
    assert_eq!(plan.splits.len(), 3);
    for old in &path[..3] {
        assert!(plan.split_of(*old).unwrap().root.is_none());
    }
    assert!(plan.split_of(path[3]).is_none());
}
