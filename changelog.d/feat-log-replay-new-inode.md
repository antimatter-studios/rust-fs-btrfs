### Added

- **A log that creates a file is replayed** (#376). A file created and made
  durable by `fsync` alone exists only in the log, as its inode, its
  reference to its directory and its inline data. A read-write mount now
  inserts it, enters its name in the directory's two indexes from that
  reference, and grows the directory's size, as the kernel's replay does.
