### Added

- **Files for salvage: inodes, their names, their extents and their bytes.**
  - `salvage::collect_inodes` walks a filesystem tree past its damage. For every inode it gathers the inode item, every name that claims it (from the parent's `DIR_INDEX` and from its own `INODE_REF`) and every extent item, each with the verdict of the leaf it came from.
  - `salvage::FileExtentItem` decodes an `EXTENT_DATA` item, and `salvage::read_file_extent` returns the bytes one item contributes: decompressed, windowed, zeros for holes and preallocation, and zeros with a `DataFault` where the data could not be read or decoded.
  - `salvage::read_data` reads logical bytes from any copy, sector by sector where needed.
  - `salvage::copy_file` writes a file to a caller's sink from its extents, and reports the ranges no extent covered.
