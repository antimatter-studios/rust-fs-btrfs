### Added

- **The C ABI creates an empty subvolume (#267).** `fs_btrfs_subvol_create`
  makes one at a path in a directory of the top-level subvolume, with the
  requested mode and owned like the directory it is made in, and returns
  the new subvolume's id. A handle opened on another subvolume, a volume
  with quotas enabled, or a top level carrying a `btrfs.*` property to
  inherit is refused with ENOTSUP.
