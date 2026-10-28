### Added

- **A subvolume is snapshotted through the C ABI and the command line
  (#267).** `fs_btrfs_subvol_snapshot(fs, source, path, read_only)` and
  `fs.btrfs subvolume snapshot [--read-only] SOURCE PATH` reach the
  library's `create_snapshot`, the source named by the path of its top
  directory as `btrfs subvolume snapshot` names it. The C call returns the
  snapshot's id, or 0 with the error recorded; the command reports the id,
  and exits 4 for a source that is no subvolume, a name already there or a
  missing parent, and 3 for what the library refuses: a read-only top
  level, a volume with quotas enabled or a source whose tree is taller
  than one leaf (#700).
