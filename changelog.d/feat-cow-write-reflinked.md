### Added

- **A copy-on-write write into an extent a reflink shares copies it and
  keeps the other references (#261).** The write refused an extent with
  more than one reference. When the extent is newer than the last
  snapshot, every reference is a file item: the write now copies the
  item's bytes into a new extent and drops only that item's inline back
  reference from the old extent's record, which keeps its bytes, checksums
  and space for the reflink, or for another item of the same file outside
  the write. An extent every reference of which the write replaces is
  released once, however many of its items named it. An extent a snapshot
  may share is still refused.
