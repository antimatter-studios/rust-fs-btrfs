### Added

- **A copy-on-write write at the start or end of a preallocated item
  (#536).** It was refused. The bytes are written where the preallocated
  extent lies and the item splits in two over the same extent, a regular
  item for the written sectors and a preallocated one for the rest, with
  the extent's reference counted up and only the written sectors
  checksummed, as the kernel does; a write into the middle of a
  preallocated item is still refused.
