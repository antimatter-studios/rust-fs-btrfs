# The three API layers, mapped onto this crate

Every rust-fs driver exposes the same three layers, defined as traits in
rust-fs-core:

| Layer | What it does |
|---|---|
| **VolumeAPI** | Open, bytes, geometry, superblock and its copies, health of the volume as a whole |
| **PrimitiveAPI** | Read-only access to the on-disk structures that survives damage, plus the salvage verbs |
| **FileAPI** | Browse, read and copy on a healthy volume |

This page records what the crate already has for each layer, where it
stops short, and the work that closes each gap. It was surveyed at
0.11.0. Rows move to `docs/features.md` as the work lands. The tables
below are the survey; the next section says what implements each layer.

Everything below is read-only. The writable surface (`mount_rw`, the
`BlockDevice` paths) stays the crate's own and is not part of any layer.

## Where each layer is implemented

`src/layers.rs` implements rust-fs-core's traits (`fs_core::layers`, the
`layers` feature). The Btrfs-specific primitives they are built from stay in
`src/salvage/`, and nothing Btrfs-specific moves to core.

| Trait | Type | Built on |
|---|---|---|
| `VolumeAPI`, `OpenVolume` | `layers::BtrfsVolume` | `salvage::superblock_copies`, `Filesystem::mount` for `open` |
| `PrimitiveAPI` | `layers::BtrfsVolume` | superblock copies, the chunk, root and filesystem tree roots |
| `SalvageAdapter` | `layers::BtrfsAdapter` | `salvage::scan_tree_blocks`, `walk_tree`, `search_range`, `build_chunk_map`, `root_tree_candidates`, `compression::decompress` |
| `FileAPI` | `layers::BtrfsVolume`, node `BtrfsNode { tree, ino }` | `Filesystem`, one mount per subvolume |

Candidates are inode items, found by scanning node-sized windows for
filesystem tree leaves. A leaf the current trees reach gives `Live`
candidates. A leaf only an older generation or nothing reaches gives
candidates of `Unknown` state: Btrfs records no deletion, so a removed
file's last record is evidence of unknown standing. A compressed run is an
`Encoding` whose codec is `zlib`, `lzo` or `zstd`, whose `decoded` is the
extent's `ram_bytes`, and whose `offset` and `length` are the window of the
decoded extent the file extent item references.

The old API, `Filesystem` and the C ABI, is unchanged.

## VolumeAPI

| Need | Today | Gap |
|---|---|---|
| Open from a read-only source | `Filesystem::mount(Arc<dyn BlockRead>)`, `mount_pool`, `mount_pool_degraded`, `mount_ignoring_log` (`src/fs.rs`, `src/log_tree.rs`) | Mount refuses a dirty log, a metadump and a device-count mismatch outright. A salvage open has to go past them and report each one as a finding |
| Superblock | `superblock::read_superblock(&dyn BlockRead)` reads the copies at 64 KiB, 64 MiB and 256 GiB and keeps the newest valid one, saying which won | Not re-exported. An intact copy with an unsupported incompat flag stops the read |
| Superblock copies | Read, but only the winner is returned | Each copy needs its own verdict (unreadable, bad magic, bad checksum, generation), and a copy with a bad checksum still has to be decodable |
| Backup roots | Only the offset `offsets::SUPER_ROOTS` exists | The four `btrfs_root_backup` slots are never parsed. Each one names an older tree root, chunk root, extent root, fs root, csum root and device root with their generations |
| Geometry | `Superblock` fields: sector size, node size, total bytes, bytes used, device count, checksum type | None for a healthy volume |
| Devices | `DevItem::parse`, `Superblock::dev_item` | Device items in the chunk tree are only read for writes |
| Chunk map | `ChunkMap::from_sys_chunk_array`, `bootstrap`, `chunk_from_item`, `map`, `map_mirror` (`src/chunk.rs`), pure and public | The mount walks the chunk tree with `?`, so one bad chunk-tree block fails the open. A salvage open keeps the `sys_chunk_array` map and adds what the chunk tree still yields |
| Health | `Superblock::has_error_flag`, `has_dirty_log`, `is_seeding`, `is_metadump`; `check::check(&Filesystem)` | `check` needs a mounted volume. Health of an unmountable one has to come from the superblock copies and a tolerant walk |

## PrimitiveAPI

| Need | Today | Gap |
|---|---|---|
| Tree blocks | `btree::TreeBlock::parse` verifies checksum, bytenr, fsid, level and item bounds; `Header::parse` alone (`src/btree.rs`) | Every check is fatal, and the items are only returned after all pass. Salvage needs the header and items of a block whose checksum fails, with the failure as a verdict |
| Walk a tree from any root | `btree::Tree::for_each(root, …)`, `read_child(logical, generation)` with the parent transid check; `open_subvolume_at(bytenr)` | `walk` stops at the first bad block. A tolerant walk records the bad block, skips its subtree and carries on |
| Older generations | Nothing | A restore-style walk: try the current roots, then each backup root, then roots found by scanning |
| Find tree blocks on raw disk | Nothing | A scan of node-size-aligned blocks for this filesystem's fsid, keeping each header's bytenr, generation, owner and level, as `btrfs-find-root` does |
| Root, fs, extent and csum trees | Found through the root tree at mount; `root_item_target` is `pub(crate)` | Public lookups that run on a salvage open |
| Inodes, directories, xattrs | `inode::Inode::parse`, `dir::parse_dir_items`, `xattr::parse_xattr_items`, pure and public | None for decoding. They need a tolerant walk to be fed from |
| File extents | `decode_extent`, `Piece` and `read_extents` in `src/fs.rs` are crate-private; `compression::decompress` is public | No public way to decode a file extent item or read one extent without a path. Salvage reads by extent |
| Logical to physical | `ChunkMap::map*` public; `read_logical_pool` is `pub(crate)` | A public mapped read with mirror fallback that reports each failed copy |
| Checksums | `ChecksumType::verify` and `digest` are pure; data verification (`read_data_verified`) is `pub(crate)` and always on | Data checksums as a verdict on bytes read, not a refusal |

### The salvage verbs

| Verb | Btrfs meaning | Built from |
|---|---|---|
| Find candidates | Inodes and directory entries in every fs tree reachable from the current, backup and scanned roots. Each is a candidate with the block it came from and that block's checksum verdict | Tolerant walk, raw scan, item decoders |
| Walk extents | Follow an inode's `EXTENT_DATA` items through the chunk map to device byte ranges. Inline, compressed and preallocated extents are named as such | File extent decoding, chunk map |
| Rebuild the tree | Parent claims from `INODE_REF` and `DIR_INDEX` items. Every claim is kept, so a file seen under two generations shows both | Item decoders |
| Report damage | Superblock copies, backup roots that disagree, tree blocks that fail checksum, bytenr, fsid or transid, broken chunk-tree items and data checksum failures | Everything above |
| Read by extent | Read the device bytes of one extent. Compressed extents are read raw; decompression is part of materializing | Mapped read |
| Materialize | Write a plan's output to a caller's sink, never to the source | Mapped read, `compression::decompress` |
| Capabilities | Btrfs is copy-on-write and never records a deletion, so deleted candidates come from older generations. Cluster steps do not apply: a Btrfs address is a logical byte, not a cluster number | — |

## FileAPI

| Need | Today | Gap |
|---|---|---|
| Root, readdir, stat | `Filesystem::root_inode`, `read_dir(ino)`, `read_inode`, `lookup`, `lookup_path` | Only the trait mapping |
| Read and copy | `read_at(ino, offset, buf)`, `read_file` (capped at 1 GiB), `read_path` | Copy has to stream through `read_at`, not `read_file`, to lift the cap |
| Symlinks, xattrs | `read_link`, `list_xattrs`, `get_xattr` | Only the trait mapping |
| Subvolumes | `subvolumes()`, `open_subvolume(id)`, `resolve_path` | A node has to carry its subvolume, so browsing crosses subvolume boundaries |

## How it is tested

The oracle is the Linux kernel and btrfs-progs in the harness VM, as for
the rest of the crate. Salvage tests read images that `mkfs.btrfs` made and
the kernel populated, and compare what they recover with the manifest of
what the kernel wrote. The damaged variants are copies of those images with
named structures overwritten: a superblock copy, a chunk-tree block, the
current fs-tree root, a data extent. A damaged variant has to recover what
the damage left recoverable, and report the damage it made.
