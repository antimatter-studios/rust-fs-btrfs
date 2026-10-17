### Added

- **Extended references a log holds are replayed** (#407). A committed
  file with more names in one directory than its `INODE_REF` item holds
  keeps the rest in `INODE_EXTREF` items; given another name after the
  last commit and made durable by an fsync, it keeps every committed name
  and gains the new one, with the link count the log gives it, as the
  kernel's own replay leaves it.
