### Added

- **A transaction removes a tree leaf its edits empty (#546).** A change
  that left a leaf other than its tree's root with no items was refused.
  When the leaf is not its parent's first child, it now leaves its tree:
  its pointer leaves the parent node, its block is released, and nothing
  is allocated for it, in any tree a transaction edits. An emptied first
  child is still refused (#547). `tests/emptied_leaf_kernel.rs` unlinks
  the files whose items fill an fs tree leaf and has `btrfs check`,
  `dump-tree`, a kernel listing and a scrub accept the result.
