### Fixed

- **A file with thousands of extents can be removed (#724).** `fs.btrfs rm`
  refused a file whose extent records filled whole extent tree leaves,
  because the records in a leaf the removal emptied were not counted as
  released. The refusal left the volume unchanged.
