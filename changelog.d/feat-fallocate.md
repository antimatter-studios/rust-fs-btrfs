### Added

- **A range of a file can be preallocated and punched out, as `fallocate(2)`
  does (#713).** `fallocate`, `punch_hole` and `fs_btrfs_fallocate` give each
  hole of a range a preallocated extent, growing the size unless asked not to,
  and take a range out of a file, releasing each extent nothing else holds or
  dropping this file's reference to it. Part of a sector at a punched range's
  edge is zeroed, and a preallocation past a file ending partway through a
  sector rewrites that sector, each as the kernel does. Inline data, a volume
  without no-holes, a subvolume other than the top level, and part of a sector
  in a `nodatacow` file's regular extent or in a preallocated extent another
  reference may read are refused.
