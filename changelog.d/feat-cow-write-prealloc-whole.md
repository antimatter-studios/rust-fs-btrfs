### Added

- **A copy-on-write write covering a whole preallocated item (#535).** It
  was refused. The bytes are written where the preallocated extent lies,
  the item becomes regular in place, and the written sectors gain
  checksums, as the kernel does; a write into part of a preallocated item
  is still refused.
