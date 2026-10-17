### Added

- **A log that rewrites an inline file at another length, or compressed, is
  replayed** (#378). The logged inline extent is copied as logged, with its
  compression; the file's size follows it, and its `nbytes` gives back the
  old extent's bytes and counts the new one's.
