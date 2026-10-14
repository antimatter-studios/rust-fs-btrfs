### Fixed

- **fsck.btrfs counts a v1 space cache's data extents as referenced (#270).**
  On a volume without the free-space tree, the kernel saves each block
  group's free space in a file of the root tree, with data extents of its
  own; `fsck.btrfs` looked for file extents only in subvolumes and reported
  each of those as referenced by no file, on a volume `btrfs check` finds
  clean. `tests/space_cache_v1_kernel.rs` also shows a write to such a
  volume is safe: a commit leaves `cache_generation` behind, so the kernel
  discards the old caches and rebuilds them, and `btrfs check` and the
  kernel agree with what was written.
