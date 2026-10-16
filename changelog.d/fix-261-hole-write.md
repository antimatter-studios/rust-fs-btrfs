### Fixed

- **A copy-on-write write into a hole adds an extent (#261).** It was
  refused. The sectors the write touches inside the hole become a new
  extent with an `EXTENT_DATA` item of its own, the bytes around the
  write read as zeros as the hole did, and the inode's `nbytes` grows by
  the sectors added while its `size` stays.
