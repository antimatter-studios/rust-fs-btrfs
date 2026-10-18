### Added

- **A removed name whose inode keeps others is replayed** (#411). One of
  two hard-linked names of a committed file, unlinked and made durable by
  an fsync of its directory alone, leaves a log that speaks for the
  directory's names without holding the file; replay now drops that name
  and its reference record and leaves the file with one link fewer, as
  the kernel's own replay does. Such a log was refused before.
