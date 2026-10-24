### Added

- **A copy-on-write write strictly inside a preallocated item (#537).** It
  was refused. The bytes are written where the preallocated extent lies and
  the item splits in three over the same extent, preallocated before the
  write, regular for the written sectors and preallocated after, with the
  extent's reference counted up by two and only the written sectors
  checksummed, as the kernel does.
