### Fixed

- **A write starting well past an inline file's end leaves a hole, as the
  kernel does (#592).** Since #507 the inline data and the write moved
  into one extent from offset 0, storing the gap between them as zeros
  and leaving the inode's `nbytes` larger than the kernel's. A write that
  starts two sectors or more into the file now moves the inline data into
  a first-sector extent of its own and leaves the sectors before the
  write a hole: no item on a NO_HOLES volume, a hole item otherwise.
