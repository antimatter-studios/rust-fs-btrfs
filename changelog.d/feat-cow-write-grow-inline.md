### Added

- **A copy-on-write write that grows an inline file within the inline
  limit (#530).** It was refused. When the grown data still fits inline,
  the inline item is rewritten longer and the inode's size and nbytes
  move with it, and the file stays inline, as the kernel leaves it.
