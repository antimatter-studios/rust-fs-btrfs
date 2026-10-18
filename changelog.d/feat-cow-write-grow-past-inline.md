### Added

- **A copy-on-write write that grows an inline file past the inline
  limit, or starts past its end (#507).** An append the inline item could
  no longer hold was refused, and a write starting past the end within the
  first sector planned a new extent at the old, unaligned end of the file.
  The inline data now moves into a regular extent at offset 0 with the
  write, as the kernel moves it, leaving only sector-aligned extents.
