### Added

- **A transaction removes an emptied leaf that is its parent's first child
  (#547).** Such a leaf was refused. It now leaves its tree like any other
  emptied leaf, and the parent's first key becomes its next child's, carried
  up through every ancestor whose own first key it was. A change emptying
  every leaf under one node is still refused, by name. A new kernel-oracle
  test in `tests/cow_write_oracle.rs` overwrites a file whose digests alone
  fill the checksum tree's first leaf and has `btrfs check
  --check-data-csum`, `dump-tree`, the kernel's read-back and a scrub accept
  the result.
