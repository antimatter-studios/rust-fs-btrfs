### Changed

- **A seed and its sprout are opened read-write as one pool (#624).**
  `mount_pool_rw` accepts the seed device beside the filesystem sprouted from
  it, and holds the seed for reading only: it need not be writable, a commit
  writes it no superblock, a chunk stripe on it is refused rather than
  written, and a remount after a commit keeps it. Checked by mounting copies
  of the kernel-made seed and sprout in both orders, reading every file as
  the kernel wrote it, and finding the seed copy's SHA-256 unchanged.
  Placing new extents and chunks only on the sprout is #625.
