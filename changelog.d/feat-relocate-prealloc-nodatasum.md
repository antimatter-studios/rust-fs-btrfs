### Added

- **Preallocated and nodatasum data extents relocated (#496, part of
  #398).** `Filesystem::relocate_block_group` no longer refuses a data
  group holding an extent with sectors the checksum tree has no digest
  for, when only preallocated items or items of `NODATASUM` files name
  those sectors. They move with the checksums they have and none are
  invented for them; a sector a checksummed file's regular item names
  without a digest is still refused before anything is written.
