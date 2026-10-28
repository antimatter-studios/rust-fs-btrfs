### Added

- **Balance reaches callers (#402).** `Filesystem::balance` relocates
  every block group a list of `balance::Balance` filters selects, each into
  a newly allocated chunk, and reports where each went. Which groups is
  decided once, and every one is checked against what relocation refuses
  before the first moves. The C ABI gains `fs_btrfs_balance(fs, kinds,
  convert)`, with `FS_BTRFS_BALANCE_SINGLE` for a conversion to single as
  the kernel spells it: EROFS on a read-only handle, EINVAL for kinds or a
  conversion it cannot mean, ENOTSUP for what relocation refuses. The
  command-line tools gain `fs.btrfs balance -d[FILTERS] -m[FILTERS]`, with
  `convert=`, `profiles=`, `usage=` and `soft` as `btrfs balance start`
  takes them; the kernel's other filters answer `not implemented`.
