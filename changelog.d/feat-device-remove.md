### Added

- **A device is removed (#303).** `Filesystem::remove_device` takes a
  device that holds no chunk out of a pool in one committed transaction:
  its `DEV_ITEM` deleted from the chunk tree, and every other member's
  superblock counting one device fewer and its size less. Its own
  superblocks are wiped once the commit is flushed. A device holding a
  chunk is refused until balance can relocate it (#304), and so is the
  last device of a filesystem.
