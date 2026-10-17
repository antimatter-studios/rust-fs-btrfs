### Added

- **Directories a log creates are replayed** (#403). A directory made after
  the last commit and made durable by an fsync of its parent is created by
  a read-write mount's replay, with the empty files and directories the
  log names inside it, as the kernel's own replay creates them.
