### Added

- **A subvolume is deleted through the C ABI and the command line (#267).**
  `fs_btrfs_subvol_delete(fs, path)` and `fs.btrfs subvolume delete PATH`
  reach the library's lazy delete: the subvolume's entry, references and
  UUID go in one transaction and the kernel's cleaner drops its tree on the
  next read-write mount. The C call returns 0, or -1 with the error
  recorded; the command reports the deleted id, and exits 4 for a missing
  name, a name that is no subvolume or a missing parent, and 3 for what the
  library refuses: the default subvolume, one holding another, a read-only
  top level or a volume with quotas enabled.
