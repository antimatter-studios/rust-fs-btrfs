### Added

- **Logged extended attributes are replayed** (#379). For an inode the log
  holds in full, the logged attributes become its attributes: each is put
  as logged, and a committed attribute the log does not hold is deleted, as
  the kernel's replay does.
