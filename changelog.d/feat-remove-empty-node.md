### Added

- A transaction removes internal tree nodes left without children, recursively,
  and replaces an empty root node with one empty leaf (#548). The root item and
  the new block's extent record both name level zero. Released nodes leave the
  extent and free-space accounting. A kernel-oracle regression truncates the
  only checksummed file on a multi-leaf checksum tree and requires btrfs check,
  kernel readback, dump-tree, and scrub to accept the result.
