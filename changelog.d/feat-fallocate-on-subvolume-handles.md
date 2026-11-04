### Added

- **A range of a file can be preallocated and punched out through a subvolume
  handle (#735).** `fallocate`, `punch_hole` and `fs_btrfs_fallocate` on a
  handle from `fs_btrfs_open_subvolume` change the file in that subvolume, where
  they returned ENOTSUP. A read-only subvolume returns EROFS.
