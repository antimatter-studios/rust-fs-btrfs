### Fixed

- **Log replay deletes an inode whose last name is an extended
  reference** (#478). A committed file whose only name is held in an
  `INODE_EXTREF` item, that name removed after the last commit and its
  directory made durable by an fsync, left a log that replay refused. It
  now deletes the file with its extended reference and its data, as the
  kernel's own replay does.
