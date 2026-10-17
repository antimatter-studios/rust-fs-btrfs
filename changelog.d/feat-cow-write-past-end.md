### Added

- **A copy-on-write write that starts past a file's end is written (#433).**
  It was refused. As the kernel does, when the old end lies partway through
  a sector the write does not cover, that sector is copied first with zeros
  past the old end, so bytes left on disk there never read back; the gap
  is an implicit hole with no item, the write's own sectors are a new
  extent, and `size` and `nbytes` move as for an append, on a volume with
  the no-holes feature.
