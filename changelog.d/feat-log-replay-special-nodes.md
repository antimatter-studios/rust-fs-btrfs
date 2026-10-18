### Added

- **Device, FIFO and socket nodes a log creates are replayed** (#405). A
  character or block device, a FIFO or a socket made after the last commit
  and made durable by an fsync of its parent is created by a read-write
  mount's replay, with its type and device number as logged and its
  directory entry typed for it, as the kernel's own replay creates it.
