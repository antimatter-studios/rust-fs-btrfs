### Added

- **`fs.btrfs resize` grows and shrinks a filesystem (#264).** It answered
  `not implemented`. It takes a size (`20G`), a change to the current one
  (`+1G`, `-512M`) or `max`, and commits the library's resize as one
  transaction. An image file is made longer before a grow, given its
  length back if the resize is then refused, and cut to the new end after
  a shrink, which needs `--force`; so does growing a filesystem at
  `--offset`, where the partition's end cannot be seen. A chunk past the
  new end and a pool of several devices are still refused with exit 3.
