### Added

- **A copy-on-write write grows a file on a volume without the no-holes
  feature (#436).** It was refused. As the kernel does, the gap a write
  past the end leaves is recorded as a hole item from the sector after the
  old end to the sector the write starts in, and an append at the end
  adds no hole item.
