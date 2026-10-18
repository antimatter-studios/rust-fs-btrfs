### Added

- `fs.btrfs stat PATH` reports identity, type, mode, owner, links, sizes and
  all four timestamps as signed seconds plus nanoseconds, without following
  a symlink. Btrfs metadata includes the containing subvolume, named flags
  with unknown bits kept in hex, generation, transid, sequence and the
  compression property, null when absent (#412).
