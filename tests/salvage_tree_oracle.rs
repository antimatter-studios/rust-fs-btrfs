//! Salvage walks trees, and builds the chunk map, as btrfs-progs reads
//! them, and walks past a damaged block where btrfs-progs stops.
//!
//! The oracles run in the harness VM. `btrfs inspect-internal dump-tree`
//! lists every leaf and node with its address, and every item with its
//! index, key offset, data offset and size: the walk has to reach exactly
//! those, in a healthy tree, and all of them but the damaged block's in a
//! tree with one block broken. `btrfs-map-logical` says where a logical
//! block lives on the device, so the damage lands on the block the test
//! names, and `btrfs check --readonly` confirms the damage is real.

use fs_btrfs::salvage::{
    best_copy, build_chunk_map, superblock_copies, walk_tree, BlockFault, Layout, RootPointer,
};
use fs_btrfs::superblock::Superblock;
use fs_btrfs_test_support::{copy_sparse, fixture, oracle, temp_path, tree_item_header};
use fs_core::FileDevice;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::ops::ControlFlow;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

/// `(leaf, item index, key offset, data offset, data size)`.
type ItemRow = (u64, usize, Option<u64>, u32, u32);

/// What `dump-tree` printed for one tree.
#[derive(Debug, Default)]
struct TreeDump {
    leaves: BTreeSet<u64>,
    nodes: BTreeMap<u64, u8>,
    /// Each node's children: `(block, generation)`, in order.
    pointers: BTreeMap<u64, Vec<(u64, u64)>>,
    items: BTreeSet<ItemRow>,
    /// The chunk items, by logical start: length and `(devid, offset)` per
    /// stripe.
    chunks: BTreeMap<u64, (u64, Vec<(u64, u64)>)>,
}

fn word_after(line: &str, key: &str) -> Option<u64> {
    let mut words = line.split_whitespace();
    while let Some(word) = words.next() {
        if word == key {
            return number(words.next()?);
        }
    }
    None
}

fn number(text: &str) -> Option<u64> {
    let text = text.trim_end_matches(')');
    match text.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}

fn dump_tree(image: &Path, tree: &str) -> (bool, String) {
    let out = oracle("btrfs")
        .args(["inspect-internal", "dump-tree", "-t", tree])
        .arg(image)
        .output();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    (out.status.success(), text)
}

fn parse_tree(text: &str) -> TreeDump {
    let mut dump = TreeDump::default();
    let mut leaf = None;
    let mut node = None;
    let mut chunk = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("leaf ") {
            if rest.contains(" items ") {
                let bytenr = number(rest.split_whitespace().next().unwrap()).unwrap();
                dump.leaves.insert(bytenr);
                leaf = Some(bytenr);
                node = None;
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("node ") {
            if rest.contains(" level ") {
                let bytenr = number(rest.split_whitespace().next().unwrap()).unwrap();
                dump.nodes
                    .insert(bytenr, word_after(trimmed, "level").unwrap() as u8);
                leaf = None;
                node = Some(bytenr);
            }
            continue;
        }
        if let (Some(at), true) = (node, trimmed.starts_with("key (")) {
            let block = word_after(trimmed, "block").unwrap();
            let generation = word_after(trimmed, "gen").unwrap();
            dump.pointers
                .entry(at)
                .or_default()
                .push((block, generation));
            continue;
        }
        if let (Some(at), Some(rest)) = (leaf, trimmed.strip_prefix("item ")) {
            if rest.contains(" itemoff ") {
                let index = number(rest.split_whitespace().next().unwrap()).unwrap() as usize;
                let key = rest
                    .split_once("key (")
                    .unwrap()
                    .1
                    .split_once(')')
                    .unwrap()
                    .0;
                let key_offset = number(key.split_whitespace().last().unwrap());
                let itemoff = word_after(rest, "itemoff").unwrap() as u32;
                let itemsize = word_after(rest, "itemsize").unwrap() as u32;
                dump.items
                    .insert((at, index, key_offset, itemoff, itemsize));
                chunk = key.contains("CHUNK_ITEM").then(|| key_offset.unwrap());
                continue;
            }
        }
        if let Some(logical) = chunk {
            if let Some(length) = word_after(trimmed, "length") {
                dump.chunks.insert(logical, (length, Vec::new()));
            }
            if trimmed.starts_with("stripe ") {
                let devid = word_after(trimmed, "devid").unwrap();
                let offset = word_after(trimmed, "offset").unwrap();
                dump.chunks
                    .get_mut(&logical)
                    .unwrap()
                    .1
                    .push((devid, offset));
            }
        }
    }
    dump
}

/// The superblock a mount would use, and the layout its chunk map gives.
fn open(image: &Path) -> (FileDevice, Superblock, Layout) {
    let device = FileDevice::open(image).expect("open the image");
    let copies = superblock_copies(&device);
    let sb = best_copy(&copies)
        .expect("a valid superblock copy")
        .superblock
        .clone()
        .unwrap();
    let scan = build_chunk_map(&device, &sb);
    let layout = Layout::new(&sb, scan.map);
    (device, sb, layout)
}

/// Every item the walk hands over, and every block it reports damaged.
fn walk(
    device: &FileDevice,
    layout: &Layout,
    root: RootPointer,
) -> (
    BTreeSet<ItemRow>,
    BTreeMap<u64, bool>,
    fs_btrfs::salvage::WalkReport,
) {
    let mut items = BTreeSet::new();
    let mut verified = BTreeMap::new();
    let report = walk_tree(device, layout, root, &mut |leaf| {
        verified.insert(leaf.logical, leaf.is_verified());
        for (index, item) in leaf.items().iter().enumerate() {
            items.insert((
                leaf.logical,
                index,
                Some(item.key.offset),
                item.offset,
                item.size,
            ));
        }
        ControlFlow::Continue(())
    });
    (items, verified, report)
}

/// Key offsets dump-tree prints as names are compared by the rest.
fn without_unnamed_offsets(
    rows: &BTreeSet<ItemRow>,
    dump: &BTreeSet<ItemRow>,
) -> BTreeSet<ItemRow> {
    let named: BTreeSet<(u64, usize)> = dump
        .iter()
        .filter(|row| row.2.is_none())
        .map(|row| (row.0, row.1))
        .collect();
    rows.iter()
        .map(|&(leaf, index, offset, itemoff, size)| {
            let offset = if named.contains(&(leaf, index)) {
                None
            } else {
                offset
            };
            (leaf, index, offset, itemoff, size)
        })
        .collect()
}

/// The root of `tree` as the root tree's `ROOT_ITEM` names it, read from
/// `dump-tree -t root`.
fn root_from_dump(image: &Path, tree: &str) -> RootPointer {
    let (ok, text) = dump_tree(image, "root");
    assert!(ok, "dump-tree -t root:\n{text}");
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if tree_item_header(line, &format!("key ({tree} ROOT_ITEM 0)")) {
            let body: Vec<&str> = lines.by_ref().take(6).collect();
            let body = body.join(" ");
            return RootPointer {
                bytenr: word_after(&body, "bytenr").unwrap(),
                generation: word_after(&body, "generation"),
                level: word_after(&body, "level").map(|l| l as u8),
            };
        }
    }
    panic!("no ROOT_ITEM for {tree} in dump-tree -t root");
}

/// The physical offsets of every copy of `logical`, from btrfs-map-logical.
fn physical_copies(image: &Path, logical: u64, nodesize: u32) -> Vec<u64> {
    let out = oracle("btrfs-map-logical")
        .args(["-l", &logical.to_string(), "-b", &nodesize.to_string()])
        .arg(image)
        .output();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "btrfs-map-logical -l {logical}: {text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let copies: Vec<u64> = text
        .lines()
        .filter_map(|line| word_after(line, "physical"))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    assert!(
        !copies.is_empty(),
        "btrfs-map-logical named no copy:\n{text}"
    );
    copies
}

/// A copy of `name` at a path the guest sees.
fn scratch_copy(name: &str, tag: &str) -> PathBuf {
    let dir = PathBuf::from(temp_path!("salvage-tree-{tag}"));
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    let image = dir.join(name);
    copy_sparse(fixture(name), &image).expect("copy the fixture");
    image
}

/// Flip the last byte of the block at each physical offset: inside the
/// first item's data, so the header and item array still decode and only
/// the checksum fails.
fn break_checksum(image: &Path, physical: &[u64], nodesize: u32) {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .unwrap();
    for &at in physical {
        let last = at + u64::from(nodesize) - 1;
        let mut byte = [0u8];
        file.read_exact_at(&mut byte, last).unwrap();
        file.write_all_at(&[byte[0] ^ 0xff], last).unwrap();
    }
}

/// On healthy kernel-made volumes of every profile, the chunk map holds
/// every chunk dump-tree lists, and the chunk and root trees walk to
/// exactly the leaves and items it prints.
#[test]
fn healthy_trees_walk_to_every_item_dump_tree_prints() {
    for name in [
        "btrfs-single.img",
        "btrfs-dup.img",
        "btrfs-mixed.img",
        "btrfs-rich.img",
    ] {
        let image = fixture(name);
        let (device, sb, layout) = open(&image);
        let scan = build_chunk_map(&device, &sb);
        assert!(scan.rejected.is_empty(), "{name}: {:?}", scan.rejected);
        assert_eq!(scan.system_array_tail, None, "{name}");
        assert!(
            scan.chunk_tree.damaged.is_empty(),
            "{name}: {:?}",
            scan.chunk_tree.damaged
        );

        let (ok, text) = dump_tree(&image, "chunk");
        assert!(ok, "{name}: dump-tree -t chunk:\n{text}");
        let chunk_dump = parse_tree(&text);
        assert_eq!(
            chunks_of(&scan.map),
            chunk_dump.chunks,
            "{name}: the chunk map"
        );

        for (tree, root) in [
            (
                "chunk",
                RootPointer {
                    bytenr: sb.chunk_root,
                    generation: Some(sb.chunk_root_generation),
                    level: Some(sb.chunk_root_level),
                },
            ),
            (
                "root",
                RootPointer {
                    bytenr: sb.root,
                    generation: Some(sb.generation),
                    level: Some(sb.root_level),
                },
            ),
            ("fs", root_from_dump(&image, "FS_TREE")),
        ] {
            let (ok, text) = dump_tree(&image, tree);
            assert!(ok, "{name}: dump-tree -t {tree}:\n{text}");
            let dump = parse_tree(&text);
            let (items, verified, report) = walk(&device, &layout, root);
            assert!(
                report.damaged.is_empty(),
                "{name} {tree}: {:?}",
                report.damaged
            );
            assert_eq!(
                verified.keys().copied().collect::<BTreeSet<_>>(),
                dump.leaves,
                "{name} {tree}: leaves"
            );
            assert_eq!(
                report.blocks as usize,
                dump.leaves.len() + dump.nodes.len(),
                "{name} {tree}: blocks"
            );
            assert!(
                !dump.items.is_empty(),
                "{name} {tree}: dump-tree printed items"
            );
            assert_eq!(
                without_unnamed_offsets(&items, &dump.items),
                dump.items,
                "{name} {tree}: items"
            );
        }
    }
}

/// `dump-tree -b`: one block, so the output stays small on a tree of
/// twenty thousand files, whose whole dump costs more than a minute to
/// carry back from the guest.
fn dump_block(image: &Path, bytenr: u64) -> TreeDump {
    let out = oracle("btrfs")
        .args(["inspect-internal", "dump-tree", "-b", &bytenr.to_string()])
        .arg(image)
        .output();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "dump-tree -b {bytenr}:\n{text}");
    parse_tree(&text)
}

/// The leaves a walk of `root` reaches, in walk order.
fn leaves_in_order(device: &FileDevice, layout: &Layout, root: RootPointer) -> Vec<u64> {
    let mut leaves = Vec::new();
    walk_tree(device, layout, root, &mut |leaf| {
        leaves.push(leaf.logical);
        ControlFlow::Continue(())
    });
    leaves
}

/// A three-level tree: the root node's children are the ones dump-tree
/// prints, and the first, middle and last leaves hold the items it prints.
#[test]
fn a_multi_level_tree_walks_through_its_nodes() {
    let image = fixture("btrfs-deep4k.img");
    let root = root_from_dump(&image, "FS_TREE");
    let (device, _, layout) = open(&image);

    let top = dump_block(&image, root.bytenr);
    assert_eq!(top.nodes.get(&root.bytenr).copied(), root.level);
    assert!(root.level.unwrap() >= 1, "the root is a node");
    let ours = fs_btrfs::salvage::read_block(
        &device,
        &layout,
        root.bytenr,
        fs_btrfs::salvage::Expect::default(),
    );
    assert!(ours.is_verified());
    let fs_btrfs::salvage::SalvagedBody::Node(pointers) = &ours.body else {
        panic!("the root decoded as a leaf");
    };
    assert_eq!(
        pointers
            .iter()
            .map(|p| (p.blockptr, p.generation))
            .collect::<Vec<_>>(),
        top.pointers[&root.bytenr]
    );

    let (items, _, report) = walk(&device, &layout, root);
    assert!(report.damaged.is_empty(), "{:?}", report.damaged);
    let leaves = leaves_in_order(&device, &layout, root);
    assert!(leaves.len() > 100, "{} leaves", leaves.len());
    for leaf in [
        leaves[0],
        leaves[leaves.len() / 2],
        leaves[leaves.len() - 1],
    ] {
        let dump = dump_block(&image, leaf);
        assert_eq!(dump.leaves, BTreeSet::from([leaf]));
        let ours: BTreeSet<ItemRow> = items.iter().filter(|r| r.0 == leaf).copied().collect();
        assert_eq!(
            without_unnamed_offsets(&ours, &dump.items),
            dump.items,
            "leaf {leaf}"
        );
    }
}

/// One leaf of the three-level tree broken in every copy: btrfs check names
/// it, and the walk reports it, still hands over its items flagged
/// unverified, and reaches every other leaf as the healthy walk did.
#[test]
fn a_leaf_broken_in_every_copy_is_reported_and_the_walk_goes_on() {
    let name = "btrfs-deep4k.img";
    let healthy = fixture(name);
    let root = root_from_dump(&healthy, "FS_TREE");
    let (device, _, layout) = open(&healthy);
    let (healthy_items, _, _) = walk(&device, &layout, root);
    let leaves = leaves_in_order(&device, &layout, root);
    let leaf = leaves[leaves.len() / 2];
    let leaf_dump = dump_block(&healthy, leaf);

    let image = scratch_copy(name, "leaf-every-copy");
    let (_, sb, _) = open(&image);
    let copies = physical_copies(&image, leaf, sb.nodesize);
    break_checksum(&image, &copies, sb.nodesize);

    let check = oracle("btrfs")
        .args(["check", "--readonly"])
        .arg(&image)
        .output();
    let said = String::from_utf8_lossy(&check.stdout).into_owned()
        + &String::from_utf8_lossy(&check.stderr);
    assert!(
        !check.status.success(),
        "btrfs check passed a broken leaf:\n{said}"
    );
    assert!(
        said.contains(&leaf.to_string()),
        "btrfs check names {leaf}:\n{said}"
    );

    let (device, _, layout) = open(&image);
    let (items, verified, report) = walk(&device, &layout, root);
    assert_eq!(report.damaged.len(), 1, "{:?}", report.damaged);
    let damage = &report.damaged[0];
    assert_eq!(damage.logical, leaf);
    assert_eq!(damage.faults, vec![BlockFault::Checksum]);
    assert!(damage.decoded);
    assert_eq!(damage.attempts.len(), copies.len());
    assert!(damage
        .attempts
        .iter()
        .all(|a| a.faults == vec![BlockFault::Checksum]));

    assert_eq!(verified.len(), leaves.len());
    for (&at, &ok) in &verified {
        assert_eq!(ok, at != leaf, "leaf {at} verified");
    }
    assert_eq!(items, healthy_items, "every item is still handed over");
    let broken: BTreeSet<ItemRow> = items.iter().filter(|r| r.0 == leaf).copied().collect();
    assert_eq!(
        without_unnamed_offsets(&broken, &leaf_dump.items),
        leaf_dump.items
    );
}

/// The first copy of the same leaf broken: the walk reads the second copy,
/// and still reports the first as damage.
#[test]
fn a_leaf_broken_in_its_first_copy_is_read_from_the_second() {
    let name = "btrfs-deep4k.img";
    let healthy = fixture(name);
    let root = root_from_dump(&healthy, "FS_TREE");
    let (device, _, layout) = open(&healthy);
    let (healthy_items, _, _) = walk(&device, &layout, root);
    let leaves = leaves_in_order(&device, &layout, root);
    let leaf = leaves[leaves.len() / 2];

    let image = scratch_copy(name, "leaf-first-copy");
    let (_, sb, layout) = open(&image);
    let copies = physical_copies(&image, leaf, sb.nodesize);
    assert_eq!(layout.copies(leaf), copies.len());
    assert!(copies.len() > 1, "deep4k keeps two copies of metadata");
    let first = layout.chunks.map(leaf).unwrap().physical;
    assert!(
        copies.contains(&first),
        "copy 0 is one btrfs-map-logical names"
    );
    break_checksum(&image, &[first], sb.nodesize);

    let (device, _, layout) = open(&image);
    let (items, verified, report) = walk(&device, &layout, root);
    assert!(
        verified.values().all(|&ok| ok),
        "every leaf verifies on some copy"
    );
    assert_eq!(report.damaged.len(), 1, "{:?}", report.damaged);
    let damage = &report.damaged[0];
    assert_eq!(damage.logical, leaf);
    assert!(damage.faults.is_empty(), "the copy used verifies");
    assert_eq!(damage.attempts[0].faults, vec![BlockFault::Checksum]);
    assert!(damage.attempts[1].faults.is_empty());
    assert_eq!(items, healthy_items);
}

/// The chunk tree's root leaf with its checksum broken in every copy: the
/// walk reports it, and its chunk items are still used, counted as coming
/// from an unverified leaf, so the map is the whole healthy map.
#[test]
fn chunk_items_from_a_leaf_whose_checksum_fails_are_used_and_counted() {
    let name = "btrfs-single.img";
    let healthy = fixture(name);
    let (_, text) = dump_tree(&healthy, "chunk");
    let healthy_chunks = parse_tree(&text);

    let image = scratch_copy(name, "chunk-tree-csum");
    let (_, sb, layout) = open(&image);
    let copies = physical_copies(&image, sb.chunk_root, sb.nodesize);
    assert_eq!(copies.len(), layout.copies(sb.chunk_root));
    break_checksum(&image, &copies, sb.nodesize);

    let (ok, said) = dump_tree(&image, "chunk");
    assert!(
        !ok || said.contains("checksum verify failed"),
        "btrfs-progs read a broken chunk tree:\n{said}"
    );

    let device = FileDevice::open(&image).unwrap();
    let scan = build_chunk_map(&device, &sb);
    assert_eq!(scan.chunk_tree.damaged.len(), 1);
    assert_eq!(scan.chunk_tree.damaged[0].logical, sb.chunk_root);
    assert_eq!(
        scan.chunk_tree.damaged[0].faults,
        vec![BlockFault::Checksum]
    );
    assert_eq!(scan.from_unverified_leaves, scan.from_chunk_tree);
    assert!(scan.from_chunk_tree > 0);
    assert_eq!(chunks_of(&scan.map), healthy_chunks.chunks);
}

/// The chunk tree's root zeroed in every copy: btrfs-progs cannot read the
/// chunk tree, the walk decodes nothing from it, and the map still holds
/// the system chunks from the superblock's array, as dump-tree listed them
/// before the damage.
#[test]
fn a_zeroed_chunk_tree_leaves_the_system_chunks() {
    let name = "btrfs-single.img";
    let healthy = fixture(name);
    let (_, text) = dump_tree(&healthy, "chunk");
    let healthy_chunks = parse_tree(&text);

    let image = scratch_copy(name, "chunk-tree-zero");
    let (_, sb, _) = open(&image);
    let copies = physical_copies(&image, sb.chunk_root, sb.nodesize);
    let file = OpenOptions::new().write(true).open(&image).unwrap();
    for &at in &copies {
        file.write_all_at(&vec![0u8; sb.nodesize as usize], at)
            .unwrap();
    }

    let (ok, said) = dump_tree(&image, "chunk");
    assert!(
        !ok || said.contains("ERROR"),
        "btrfs-progs read a zeroed chunk tree:\n{said}"
    );

    let device = FileDevice::open(&image).unwrap();
    let scan = build_chunk_map(&device, &sb);
    assert_eq!(scan.chunk_tree.damaged.len(), 1);
    assert_eq!(scan.from_chunk_tree, 0);
    assert!(scan.from_system_array > 0);
    let ours = chunks_of(&scan.map);
    assert!(!ours.is_empty());
    for (logical, chunk) in &ours {
        assert_eq!(Some(chunk), healthy_chunks.chunks.get(logical));
    }
    assert!(scan.map.chunks().iter().all(|c| c.is_system()));
}

fn chunks_of(map: &fs_btrfs::chunk::ChunkMap) -> BTreeMap<u64, (u64, Vec<(u64, u64)>)> {
    map.chunks()
        .iter()
        .map(|c| {
            (
                c.logical,
                (
                    c.length,
                    c.stripes.iter().map(|s| (s.devid, s.offset)).collect(),
                ),
            )
        })
        .collect()
}
