### Added

- **A directory whose name the log removes is replayed** (#410). A
  committed empty directory removed by a replacing rename, made durable
  by an fsync of its parent alone, leaves a log that speaks for the parent's
  names without holding the directory; replay now removes the directory
  with its items, as the kernel's own replay does. Such a log was refused
  before, and a directory that still holds names is still refused.
