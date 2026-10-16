### Added

- **Reflink and clone-range (#269).** `Filesystem::clone_range` and
  `fs_btrfs_clone_range` clone a range of one regular file into another,
  or into itself where the ranges do not overlap, as one committed
  transaction. No data is copied: the destination gains file items naming
  the source's extents, and each extent gains an inline back reference
  placed where the kernel looks for it. The kernel's range rules hold: a
  length of zero clones to the source's end, and offsets must be whole
  sectors. A destination range that already holds data, inline data, an
  extent whose references are kept as items of their own, files that
  differ in whether they checksum their data, and volumes without the
  no-holes feature are refused.
