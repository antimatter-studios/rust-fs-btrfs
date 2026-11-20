### Added

- **Tree roots for salvage: recorded, older and found on the raw device.**
  - `salvage::read_root_items` lists every `ROOT_ITEM` a root tree still yields, past damaged leaves.
  - `salvage::root_tree_candidates` lists every root tree the superblock copies name, newest generation first: each copy's own root, then its backup slots. When the current root tree is damaged, a restore-style search starts from an older one.
  - `salvage::scan_tree_blocks` scans the device in node-sized windows for blocks that carry the filesystem's fsid, intact or not, as `btrfs-find-root` does. `salvage::metadata_ranges` and `salvage::physical_to_logical` say where to scan and where a found block belongs.
