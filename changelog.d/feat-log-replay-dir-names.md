### Added

- **A log that adds names to a directory it speaks for is replayed**
  (#377). When the log holds a directory and an index range of its names,
  each logged name the directory lacks, of a file the log creates, is
  entered in both of the directory's indexes, and the directory takes its
  logged item and the size those names give it. A committed name in the
  range that the log removes or renames is still refused.
