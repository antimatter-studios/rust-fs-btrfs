### Added

- **A copy-on-write write spanning a regular extent and a preallocated item
  (#638).** It was refused. The write is split at the preallocated item's
  edge: the regular sectors go to a new extent and the preallocated ones are
  written where their extent lies, the item splitting at the write's end, as
  the kernel does.
