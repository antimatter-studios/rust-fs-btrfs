### Fixed

- **A copy-on-write copy longer than 128 KiB is stored compressed, as
  several extents (#357).** A compressed extent decodes to at most one
  compression unit, so a copy longer than that was written uncompressed
  even when the file or the mount asked for compression, and files the
  kernel had written uncompressed, whose extents are usually far longer,
  never took compression on write. Such a copy is now split into units,
  as the kernel stores a long range: the first unit takes the old item's
  place, and each of the rest is a new extent with an `EXTENT_DATA` item
  and an extent-tree record of its own. A unit that does not shrink by a
  sector is stored as it is.
