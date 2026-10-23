### Added

- **A copy-on-write write may start past a file's end (#433).** On a
  no-holes volume the rest of the old last sector is copied as zeros, as the
  kernel zeroes it before extending a file, the gap up to the write is left
  an implicit hole, and the write's own sectors are added. It used to be
  refused.
