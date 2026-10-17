### Fixed

- **A log whose inode item holds a stale `nbytes` is replayed** (#462).
  The kernel can log a file held in full with leftover bytes where its
  `nbytes` belongs, as it does for a file with extended references
  rewritten and fsynced; the replay no longer refuses such a log, and
  counts `nbytes` again from the file's data, as the kernel's own replay
  does.
