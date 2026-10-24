### Added

- A file whose extents live on the seed of a sprouted filesystem is
  overwritten copy-on-write onto the sprout: its old seed extents leave
  only the sprout's extent tree and block-group accounting, the seed is
  never written, and btrfs-progs and the guest kernel read the pair back
  (#563).
