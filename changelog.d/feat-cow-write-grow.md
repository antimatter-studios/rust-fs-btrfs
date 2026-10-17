### Added

- **A copy-on-write write grows a file from at or before its end (#432).**
  It was refused. The sectors past the old last extent become a new extent
  with an `EXTENT_DATA` item of its own, checksummed or compressed as the
  file asks, the bytes past the old end read as zeros, and the inode's
  `size` moves to where the write stops while `nbytes` grows by the
  sectors added, in one committed transaction, on a volume with the
  no-holes feature. A write that starts past the end, leaving a gap, and
  growing a file on a volume without no-holes are still refused.
