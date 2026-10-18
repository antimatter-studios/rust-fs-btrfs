//! Synthetic planner shapes; the independent on-disk check is in
//! tests/node_split_kernel.rs. These do not validate Btrfs encoding.

use super::*;
use crate::btree::KeyPtr;
use crate::chunk::DiskKey;
use crate::leaf_edit::OwnedItem;
use crate::tree_write::{self, BlockIdentity, LeafItem};
use fs_core::{BlockDevice, BlockRead};
use std::sync::{Arc, Mutex};

/// Synthetic removal shapes isolate planning and rendering. The on-disk
/// encoding and accounting oracle is in cow_write_oracle.rs (#548).
fn emptied_tree(keep_right: bool) -> (Filesystem, Plan, DataWrite, u64, Vec<u64>) {
    let dev = Arc::new(Sparse::default());
    crate::mkfs::format(dev.as_ref(), &crate::mkfs::Options::default()).unwrap();
    let fs = Filesystem::mount(dev.clone() as Arc<dyn BlockRead>).unwrap();
    let root = fs.tree_root(objectid::FS_TREE).unwrap();
    let block = fs.read_tree_block(root).unwrap();
    let id = BlockIdentity {
        bytenr: root,
        owner: objectid::FS_TREE,
        generation: fs.sb.generation,
        level: 3,
        flags: tree_write::flags_for_new_block(),
        chunk_tree_uuid: tree_write::chunk_tree_uuid_of(block.bytes()),
    };
    let group = fs
        .block_groups()
        .unwrap()
        .into_iter()
        .find(|g| g.holds_metadata())
        .unwrap();
    let mut next = group.start + group.length / 2;
    let mut plan = Plan::default();
    let mut data = DataWrite {
        root: objectid::FS_TREE,
        ..Default::default()
    };
    let mut gone = Vec::new();
    // Two chains, each ending in one leaf. Emptying the left chain
    // must remove two internal nodes before reaching a root with a survivor.
    let mut root_ptrs = Vec::new();
    for side in 0..2 {
        let first = key(1 + side * 2);
        let mut child = 0;
        for level in 0..3 {
            let at = next;
            next += u64::from(fs.sb.nodesize);
            let id = BlockIdentity {
                bytenr: at,
                level,
                ..id
            };
            let bytes = if level == 0 {
                if side == 0 || !keep_right {
                    data.edits.push(ItemEdit::Delete(first));
                    gone.push(at);
                }
                tree_write::build_leaf(
                    &fs.sb,
                    id,
                    &[LeafItem {
                        key: first,
                        data: &[1],
                    }],
                )
                .unwrap()
            } else {
                if side == 0 || !keep_right {
                    gone.push(at);
                }
                tree_write::build_node(
                    &fs.sb,
                    id,
                    &[KeyPtr {
                        key: first,
                        blockptr: child,
                        generation: id.generation,
                    }],
                )
                .unwrap()
            };
            for mirror in 0..fs.map.mirrors_at(at).unwrap() {
                dev.write_at(fs.map.map_mirror(at, mirror).unwrap().physical, &bytes)
                    .unwrap();
            }
            if side == 0 || !keep_right {
                plan.rewrites.push(Rewrite {
                    old: at,
                    new: at + (8 << 20),
                    owner: id.owner,
                    level,
                });
            }
            child = at;
        }
        root_ptrs.push(KeyPtr {
            key: first,
            blockptr: child,
            generation: id.generation,
        });
    }
    let bytes = tree_write::build_node(&fs.sb, id, &root_ptrs).unwrap();
    for mirror in 0..fs.map.mirrors_at(root).unwrap() {
        dev.write_at(fs.map.map_mirror(root, mirror).unwrap().physical, &bytes)
            .unwrap();
    }
    plan.rewrites.push(Rewrite {
        old: root,
        new: next,
        owner: id.owner,
        level: id.level,
    });
    // Give the synthetic blocks extent records, so a full render can
    // check both their releases and the collapsed root's new level.
    let extent_root = fs.tree_root(objectid::EXTENT_TREE).unwrap();
    let block = fs.read_tree_block(extent_root).unwrap();
    let mut items: Vec<OwnedItem> = block
        .body
        .items()
        .unwrap()
        .iter()
        .map(|item| OwnedItem {
            key: item.key,
            data: block.item_data(item).unwrap().to_vec(),
        })
        .collect();
    for rewrite in &plan.rewrites {
        items.retain(|item| item.key.objectid != rewrite.old);
        let (key, body) = crate::extent_write::record_tree_block(
            &fs.sb,
            crate::extent_write::TreeBlockAllocation {
                bytenr: rewrite.old,
                level: rewrite.level,
                generation: id.generation,
                owner: rewrite.owner,
            },
        )
        .unwrap();
        items.push(OwnedItem {
            key,
            data: body.to_vec(),
        });
    }
    items.sort_by_key(|item| (item.key.objectid, item.key.key_type, item.key.offset));
    let items: Vec<_> = items.iter().map(OwnedItem::as_leaf_item).collect();
    let bytes = tree_write::build_leaf(
        &fs.sb,
        BlockIdentity {
            bytenr: extent_root,
            owner: objectid::EXTENT_TREE,
            level: 0,
            ..id
        },
        &items,
    )
    .unwrap();
    for mirror in 0..fs.map.mirrors_at(extent_root).unwrap() {
        dev.write_at(
            fs.map.map_mirror(extent_root, mirror).unwrap().physical,
            &bytes,
        )
        .unwrap();
    }
    for (i, (old, owner)) in [
        (extent_root, objectid::EXTENT_TREE),
        (fs.sb.root, objectid::ROOT_TREE),
    ]
    .into_iter()
    .enumerate()
    {
        plan.rewrites.push(Rewrite {
            old,
            new: next + (i as u64 + 1) * u64::from(fs.sb.nodesize),
            owner,
            level: 0,
        });
    }
    drop(fs);
    (
        Filesystem::mount(dev as Arc<dyn BlockRead>).unwrap(),
        plan,
        data,
        root,
        gone,
    )
}

#[test]
fn emptied_nodes_leave_recursively_and_the_next_child_supplies_the_first_key() {
    let (fs, mut plan, data, root, gone) = emptied_tree(true);
    fs.plan_removals(&mut plan, &data).unwrap();
    assert_eq!(
        plan.removed.iter().map(|r| r.old).collect::<BTreeSet<_>>(),
        gone.into_iter().collect()
    );
    assert_eq!(plan.rewrites.len(), 3);
    let blocks = fs
        .render_plan_with(&plan, &data, fs.sb.generation + 1)
        .unwrap();
    let new_root = blocks
        .iter()
        .find(|b| b.logical == plan.rewrites[0].new)
        .unwrap();
    let geom = crate::btree::TreeGeometry::from_superblock(&fs.sb);
    let block =
        crate::btree::TreeBlock::parse(new_root.bytes.clone(), new_root.logical, &geom).unwrap();
    assert_eq!(block.header.level, 3);
    let ptrs = block.body.key_ptrs().unwrap();
    assert_eq!(ptrs.len(), 1);
    assert_eq!(ptrs[0].key, key(3));
    assert_eq!(plan.rewrites[0].old, root);
}

#[test]
fn an_emptied_root_node_becomes_one_empty_leaf() {
    let (fs, mut plan, data, root, gone) = emptied_tree(false);
    fs.plan_removals(&mut plan, &data).unwrap();
    assert_eq!(plan.removed.len(), gone.len());
    assert_eq!(plan.rewrites.len(), 3);
    assert_eq!(plan.rewrites[0].old, root);
    let blocks = fs
        .render_plan_with(&plan, &data, fs.sb.generation + 1)
        .unwrap();
    assert_eq!(blocks.len(), 3);
    let geom = crate::btree::TreeGeometry::from_superblock(&fs.sb);
    let new_root = plan.rewrites[0].new;
    let rendered = blocks.iter().find(|b| b.logical == new_root).unwrap();
    let block =
        crate::btree::TreeBlock::parse(rendered.bytes.clone(), rendered.logical, &geom).unwrap();
    assert_eq!(block.header.level, 0);
    assert!(block.body.items().unwrap().is_empty());
    let extent = plan
        .rewrites
        .iter()
        .find(|r| r.owner == objectid::EXTENT_TREE)
        .unwrap();
    let rendered = blocks.iter().find(|b| b.logical == extent.new).unwrap();
    let extent =
        crate::btree::TreeBlock::parse(rendered.bytes.clone(), rendered.logical, &geom).unwrap();
    let root_record = extent
        .body
        .items()
        .unwrap()
        .iter()
        .find(|i| i.key.objectid == new_root)
        .unwrap();
    assert_eq!(root_record.key.key_type, key_type::METADATA_ITEM);
    assert_eq!(root_record.key.offset, 0);
    assert!(!extent
        .body
        .items()
        .unwrap()
        .iter()
        .any(|i| i.key.objectid == root));
    let root_tree = plan
        .rewrites
        .iter()
        .find(|r| r.owner == objectid::ROOT_TREE)
        .unwrap();
    let rendered = blocks.iter().find(|b| b.logical == root_tree.new).unwrap();
    let root_tree =
        crate::btree::TreeBlock::parse(rendered.bytes.clone(), rendered.logical, &geom).unwrap();
    let item = root_tree
        .body
        .items()
        .unwrap()
        .iter()
        .find(|i| i.key.objectid == objectid::FS_TREE && i.key.key_type == ROOT_ITEM_KEY)
        .unwrap();
    let body = root_tree.item_data(item).unwrap();
    assert_eq!(body[root_item::LEVEL], 0);
    assert_eq!(
        u64::from_le_bytes(
            body[root_item::BYTENR..root_item::BYTENR + 8]
                .try_into()
                .unwrap()
        ),
        new_root
    );
    assert_eq!(
        plan.usage_delta(u64::from(fs.sb.nodesize)),
        -(gone.len() as i128) * i128::from(fs.sb.nodesize)
    );
}

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

/// A record and its keyed references can start different leaves with
/// the same objectid. Address routing must select the record's leaf.
#[test]
fn an_extent_record_precedes_a_leaf_starting_with_its_keyed_references() {
    let dev = Arc::new(Sparse::default());
    crate::mkfs::format(dev.as_ref(), &crate::mkfs::Options::default()).unwrap();
    let fs = Filesystem::mount(dev.clone() as Arc<dyn BlockRead>).unwrap();
    let group = fs
        .block_groups()
        .unwrap()
        .into_iter()
        .find(|g| g.holds_metadata())
        .unwrap();
    let root = group.start + group.length / 2;
    let size = u64::from(fs.sb.nodesize);
    let record = root + size;
    let refs = record + size;
    let original = fs
        .read_tree_block(fs.tree_root(objectid::EXTENT_TREE).unwrap())
        .unwrap();
    let id = BlockIdentity {
        bytenr: root,
        owner: objectid::EXTENT_TREE,
        generation: fs.sb.generation,
        level: 1,
        flags: tree_write::flags_for_new_block(),
        chunk_tree_uuid: tree_write::chunk_tree_uuid_of(original.bytes()),
    };
    let record_key = DiskKey {
        objectid: 1 << 20,
        key_type: key_type::EXTENT_ITEM,
        offset: 65536,
    };
    let ref_key = DiskKey {
        key_type: crate::backref::EXTENT_DATA_REF,
        offset: 123,
        ..record_key
    };
    let mut ptrs = Vec::new();
    for (at, key) in [(record, record_key), (refs, ref_key)] {
        let leaf = tree_write::build_leaf(
            &fs.sb,
            BlockIdentity {
                bytenr: at,
                level: 0,
                ..id
            },
            &[LeafItem { key, data: &[1] }],
        )
        .unwrap();
        for mirror in 0..fs.map.mirrors_at(at).unwrap() {
            dev.write_at(fs.map.map_mirror(at, mirror).unwrap().physical, &leaf)
                .unwrap();
        }
        ptrs.push(KeyPtr {
            key,
            blockptr: at,
            generation: id.generation,
        });
    }
    let node = tree_write::build_node(&fs.sb, id, &ptrs).unwrap();
    for mirror in 0..fs.map.mirrors_at(root).unwrap() {
        dev.write_at(fs.map.map_mirror(root, mirror).unwrap().physical, &node)
            .unwrap();
    }
    assert_eq!(
        fs.leaves_holding(root, &[record_key.objectid]).unwrap(),
        BTreeSet::from([record])
    );
}

/// A collision probe matches a keyed reference by every field of its
/// body, so a reference that differs in one field, or a short body, is
/// passed over rather than dropped in its place (#608).
#[test]
fn a_keyed_reference_is_matched_by_its_root_inode_and_offset() {
    let body = |tree: u64, ino: u64, offset: u64| {
        let mut b = Vec::new();
        for v in [tree, ino, offset] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&1u32.to_le_bytes());
        b
    };
    assert!(keyed_data_ref_names(&body(5, 300, 65536), 5, 300, 65536));
    assert!(!keyed_data_ref_names(&body(6, 300, 65536), 5, 300, 65536));
    assert!(!keyed_data_ref_names(&body(5, 301, 65536), 5, 300, 65536));
    assert!(!keyed_data_ref_names(&body(5, 300, 69632), 5, 300, 65536));
    assert!(!keyed_data_ref_names(
        &body(5, 300, 65536)[..20],
        5,
        300,
        65536
    ));
}
