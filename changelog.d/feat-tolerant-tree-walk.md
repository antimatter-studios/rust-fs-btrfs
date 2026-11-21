### Added

- **Tree blocks are read whatever their state, for salvage.** `salvage::read_block` tries every copy of a block and returns the first that verifies. If none does, it returns the first copy it could read, with its header and whatever of its items or child pointers still decode, and names every check each copy failed: unmapped, on another device, unreadable, checksum, address, fsid, generation, level or a malformed body.
- **Trees are walked past their damage.** `salvage::walk_tree` hands every leaf to the caller, damaged or not, from any root. It records each damaged block and goes on with the rest of the tree, and reads each address once, so a cycle cannot loop it.
- **The chunk map is built from what survives.** `salvage::build_chunk_map` keeps every chunk the system chunk array and the chunk tree still yield, and names those it could not use. Chunks read from a leaf that failed a check are used and counted.
