### Added

- **A file is defragmented through the C ABI and `fs.btrfs defragment` (#305).**
  `fs_btrfs_defragment` rewrites each run of a regular file's adjacent
  extents shorter than a target (0 for the default, 32 MiB) as one
  extent, copy-on-write, and fills a `fs_btrfs_defrag_t` with the file's
  extent items before and after, the runs rewritten and the bytes copied.
  `fs.btrfs defragment [--target SIZE] PATH` does the same and reports
  those counts. A missing path, or one that is not a regular file, is
  ENOENT or EISDIR, exit 4; a target shorter than a sector and an extent a
  snapshot may share are ENOTSUP, exit 3.
