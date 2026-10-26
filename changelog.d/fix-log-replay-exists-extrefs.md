### Fixed

- **A file logged only to say it exists keeps its extended references
  in its link count** (#476). A committed file with `INODE_EXTREF`
  items, given a name in another directory and made durable by an fsync
  of that directory, is logged without them; replay counted its names
  from its `INODE_REF` items alone and wrote a link count short by one
  for each extended reference. It now keeps and counts them, as the
  kernel's own replay does.
