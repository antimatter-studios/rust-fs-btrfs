### Fixed

- **Unlink and rename remove a name held in an extended reference (#424).**
  A file with more hard links in one directory than its `INODE_REF` item
  holds keeps the rest in `INODE_EXTREF` items; removing or renaming one of
  those names was refused. It now leaves its `INODE_EXTREF` item, which is
  deleted with its last record, as the kernel does.
