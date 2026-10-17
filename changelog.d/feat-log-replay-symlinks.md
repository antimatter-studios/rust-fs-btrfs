### Added

- **Symbolic links a log creates are replayed** (#404). A symbolic link
  made after the last commit and made durable by an fsync of its parent is
  created by a read-write mount's replay, with its target as logged and its
  directory entry typed as a link, as the kernel's own replay creates it.
