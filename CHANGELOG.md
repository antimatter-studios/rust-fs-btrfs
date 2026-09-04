# Changelog

Notable changes to `am-fs-btrfs`, newest first. This is a `0.x` crate, so the
**minor** is the compatibility boundary: a minor bump may break API, a patch
never does.

## [Unreleased]

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
