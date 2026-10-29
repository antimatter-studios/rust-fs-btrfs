### Added

- **Log replay deletes a symbolic link, device, FIFO or socket whose last
  name the log removes (#394).** A read-write mount replays such a log as
  the kernel does, deleting the inode with all its items, where it refused
  the log with `Error::DirtyLog` before.
