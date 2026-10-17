### Added

- **A log of data and names is replayed** (#375, #377). A read-write mount
  now replays a write inside a committed extent, which keeps the extent's
  head and tail as references into it, an append, and a truncate, cutting
  the file at its logged size. A file the log creates may have regular
  extents. In a directory the log speaks for, a committed name the log
  lacks is removed and a renamed inode takes its logged name; an inode that
  loses its last name is deleted with its data. Data whose bytes do not
  match the digests the log carries is refused before anything is written.
