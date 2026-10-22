### Added

- **Snapshot-shared data extents relocate with their subvolumes (#401).**
  Balance follows shared leaf references across live subvolumes, checks their
  item counts, and converts them to ordinary data references as copy-on-write
  gives each subvolume its own leaf.
