### Added

- **A data block group's extents enumerated (#396, part of #304).**
  `Filesystem::block_group_extents` lists every data extent of a data
  block group, in address order, each with every `EXTENT_DATA` item that
  names it (tree, inode, file offset, offset into the extent, length),
  found through the extent's keyed back references and checked against
  its reference count: the first step of relocating a block group. A
  metadata or system group, a shared data reference, references that do
  not add up, and a volume where a subvolume has been snapshotted are
  refused by name.
