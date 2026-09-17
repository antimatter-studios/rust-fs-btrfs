# Changelog

Notable changes to `am-fs-btrfs`, newest first. This is a `0.x` crate, so the
**minor** is the compatibility boundary: a minor bump may break API, a patch
never does.

## [Unreleased]

### Added

- **A path can cross into subvolumes.** `Filesystem::resolve_path` walks a
  path through any number of subvolume boundaries and returns the inode
  together with the read-only handle of the tree it belongs to
  (`PathTarget`). `read_path` and `list_path` use it, so `/sub/inner/c.txt`
  reads. A crossing follows the kernel's rules: the entry must be backed by
  a `ROOT_REF` from the tree it sits in. When one is missing, which is how
  a snapshot's copy of a nested subvolume's entry looks, the result is the
  empty directory the kernel shows there (inode 2), not the live
  subvolume. `lookup` and `lookup_path` still stop at a boundary, and their
  refusal now points at `resolve_path` (#62).
- **Extended attributes can be read.** `XATTR_ITEM` was parsed as far as
  its shape and the value thrown away, so an attribute sitting in the
  tree could not be reached from anywhere. `Filesystem::list_xattrs` and
  `Filesystem::get_xattr` return names and values, and the C ABI gains
  `fs_btrfs_listxattr` and `fs_btrfs_getxattr` with the same signatures
  and semantics `fs_ext4_*` already has.
  - Names that hash to the same key share one item, and every record in
    it is returned. `tests/xattr_oracle.rs` sets two such names — found
    by searching the hash — and requires both to come back.
  - Btrfs stores the namespace prefix as part of the name, so nothing is
    expanded on the way out: `trusted.x` comes back spelled in full.
  - A zero-length value is distinct from an absent attribute, in both
    APIs.
- A fixture carrying attributes, built by
  `scripts/build-xattr-fixtures.sh` from a mounted filesystem, with
  `getfattr`'s own dump recorded beside it as the reference answer.

### Changed

- **Mounting no longer reads the whole filesystem tree.** The fs tree was
  loaded into memory at mount -- 2937 device reads and 12 MB for 20,000
  files before the first question. Items are now found by descending the
  tree when asked, with the verified blocks a descent reads kept in a
  node cache of `NODE_CACHE_BLOCKS` (1024) that a commit clears
  (`btree::Tree::with_cache`). The same volume mounts
  in 5 reads; a later path resolution through directories already listed
  makes none.
- **A lookup fetches the name by key.** `Filesystem::lookup` listed the whole
  directory and scanned it for the name, so each path component cost the
  size of its directory: 20,000 lookups in a 20,000-entry directory took
  96 s. It now reads the `DIR_ITEM` filed under the name's hash and matches
  within it (names that share a hash are packed there), 20 ms for the same
  work. `.` and `..` still resolve to nothing, as before.
- `dir::parse_dir_items` and the new `xattr::parse_xattr_items` share one
  record walk, so the bounds arithmetic over a packed
  `struct btrfs_dir_item` sequence exists once rather than twice.

### Fixed

- **A new tree block is never placed on a superblock copy.** The extent
  tree doesn't record the superblock copies, and the allocator took its
  gaps as free. On a 256 MiB volume it handed out the block whose DUP copy
  sits at 64 MiB, where a commit then writes the second superblock.
  `plan_transaction` and `find_metadata_block` now skip the `stripe_len`
  row holding each copy, as the kernel does (#175).
- **A commit fills its backup-root slot.** The superblock keeps the roots of
  the last four commits for `btrfs rescue` and `usebackuproot`, and a commit
  by this driver left its slot describing an older commit the kernel made.
  `Filesystem::commit` now reads the extent, filesystem, device and checksum
  roots out of the root tree it writes and fills the slot for its
  generation (`super_write::write_backup`), and refuses, before writing
  anything, a root tree that does not parse.
- **An in-place write no longer overwrites data a snapshot still reads.**
  `write_at` took an extent item's single reference to mean one reader. A
  snapshot of a subvolume whose tree is a node leaves data extents at
  `refs 1`, so a `chattr +C` file's write changed the snapshot's copy as
  well. The write and `can_write_in_place` now also refuse an extent from
  a generation at or before the fs tree's `last_snapshot`, as the kernel's
  nocow check does (#173).

## [0.6.2] — 2026-09-06

### Fixed

- A tree walk visits each block once. Without a visited set, a chunk
  tree that points back into itself made the mount loop.
- Extent lengths are bounded before they become allocation sizes, and a
  chunk cannot send a write outside the device it maps.

## [0.6.1] — 2026-09-04

### Changed

- **"Find a tree's root" means one thing.** Three lookups implemented it
  differently — disagreeing on the search bound and on the match order — so
  which one you called decided what you got. The visitors are now handed an
  already-parsed block rather than each re-parsing it.
- **One definition of the endian readers**, instead of a set per module.
- Refusals name the way out rather than ending in a dead end. An error that
  says only "unsupported" leaves the caller with nothing to do next.

## [0.6.0] — 2026-08-29

### Added

- **A whole transaction is written and the kernel judges it** — the plan is
  closed over its own bookkeeping, turned into the blocks it says to write, and
  then handed to `btrfs check`.
- **The free-space tree is kept in step**, and the result passes `btrfs check`.
  Settled first why the tree names more block groups than exist, which was a
  property of how it lays its items out rather than a bug.
- **Pool reads: every device is opened and each mapping is answered from the
  right one.** Previously a pool member could be read as if it were the whole
  filesystem.

### Fixed

- **One device of a multi-device pool is refused rather than read as the whole
  thing.** Reading it standalone returns whatever that disk happens to hold and
  invents the rest.
- Stripe floors that the toolchain bump revealed.
- A docblock that described behaviour the code did not have, seven test suites
  that were never being run, and three copies of one offset.

### Changed

- Pinned toolchain moves to 1.95.0, in lockstep with the rest of the family.

## [0.5.0] — 2026-08-26

### Added

- **The write path, exposed through the C ABI**: `nodatacow` files can be
  overwritten in place. CoW writes are not covered — see the README.

## [0.4.0] — 2026-08-25

### Added

- **Compressed extents are decoded** — zlib, LZO and zstd.

## [0.3.0] — 2026-08-25

### Added

- Initial public release: superblock parser, chunk address map, B-tree node and
  leaf reads with tree walking, inode and directory items, and the filesystem
  handle (mount, resolve, list, read).
- `fs_core` mounting and a C ABI aligned with the sibling drivers.
- Compression flags are accepted at mount, with the refusal paths covered.
- **Cross-validation against real media**, including multi-level tree descent,
  which is what caught the `metadata_uuid` handling. The oracle comparison runs
  before mounting, and mounts operate on copies.

### Fixed

- `readlink` refuses a buffer too small for the target instead of truncating
  the path silently.
- Both `metadata_uuid` renderings the reference tooling emits are tolerated.

[Unreleased]: https://github.com/antimatter-studios/rust-fs-btrfs/compare/v0.6.1...HEAD
[0.6.1]: https://github.com/antimatter-studios/rust-fs-btrfs/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/antimatter-studios/rust-fs-btrfs/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/antimatter-studios/rust-fs-btrfs/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/antimatter-studios/rust-fs-btrfs/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/antimatter-studios/rust-fs-btrfs/releases/tag/v0.3.0
