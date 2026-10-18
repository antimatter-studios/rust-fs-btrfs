### Added

- **Reflinked and partially overwritten data extents relocated (#495,
  part of #398).** `Filesystem::relocate_block_group` no longer refuses
  a data group holding an extent more than one file item names: a
  reflink, a clone within one file, or the bookends a partial overwrite
  leaves. The extent is copied once and every file item naming it is
  re-pointed at the new address with its offset into the extent
  unchanged, and the extent item's inline and keyed back references move
  as they are.
