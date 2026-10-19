### Added

- **A growing copy-on-write write into space preallocated past the end
  (#603).** When one item `fallocate --keep-size` left past the end of a
  file holds the old end, any gap before the write and the write itself,
  the bytes are written where that extent lies, as a write within the size
  is, the gap stays preallocated and only the size moves, as the kernel
  does. A sector-unaligned old end, a write ending off a sector inside the
  item, or one reaching past it is still refused.
