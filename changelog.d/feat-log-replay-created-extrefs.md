### Added

- **A file the log creates is named from its extended references**
  (#477). A file created after the last commit and given more names in
  one directory than its `INODE_REF` item holds, made durable by an fsync
  of that directory, is logged only to say it exists, with the rest of
  its names in `INODE_EXTREF` items; replay refused such a log. It now
  creates the file with every name and the link count the log gives it,
  and the generation of the replaying transaction, as the kernel's own
  replay does.
