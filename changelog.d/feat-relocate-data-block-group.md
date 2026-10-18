### Added

- **A data block group of plain checksummed extents relocated in one
  transaction (#493, part of #398).** `Filesystem::relocate_block_group`
  now moves a populated data group when every extent in it is named by
  exactly one file item, is checksummed in every sector, and there are at
  most 64 of them: each extent's bytes are read verified against their
  digests and written into the new chunk, then one commit re-points the
  file items, moves the extent items and their keyed back references,
  files the checksums under the new sectors, and moves the free-space
  tree and both groups' `used` with them. The emptied group is then
  removed. Shared, unchecksummed or more numerous extents are still
  refused before anything is written.
