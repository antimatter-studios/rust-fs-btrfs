### Changed

- **A clone replaces what the destination range holds (#269).**
  `clone_range` and `fs_btrfs_clone_range` no longer refuse a destination
  range that already holds data: as the kernel's `FICLONERANGE` does, an
  item wholly inside the range is deleted and its extent released, or one
  reference dropped from it when something else still holds one; an item
  running into or out of the range is cut short or started again; and an
  item running through it is split in two, both halves naming its extent.
