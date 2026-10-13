### Fixed

- **Unlinking a file's last name releases its data (#262).** `unlink`,
  `fs_btrfs_unlink` and `fs.btrfs rm` refused the last name of a file that
  still held data extents. They now release the file's extents, their back
  references, their checksums and their block groups' counts in the same
  transaction as the names, as truncate releases them. Data a snapshot or a
  reflink shares is still refused, as is replacing such a file's last name
  by rename. `tests/namespace_kernel.rs` has `btrfs check` find the volume
  clean and the kernel find the file gone.
