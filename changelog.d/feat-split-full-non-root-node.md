### Added

- A full non-root tree node splits when a leaf split needs another key pointer
  and the node above has room (#514). Both halves are recorded at the original
  level and the parent points at the new half without growing the tree. The
  kernel regression checks a 4 KiB-node volume with a level-2 root over three
  level-1 nodes, a clean `btrfs check`, and every created name read by Linux.
  Cascading splits and growing the root or chunk tree remain unsupported.
