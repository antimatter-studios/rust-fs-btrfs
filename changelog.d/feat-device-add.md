### Added

- **A device is added to a filesystem (#303).** `Filesystem::add_device`
  makes a device a member of a single-device volume or of a pool, in one
  committed transaction: a `DEV_ITEM` for it in the chunk tree, the
  device count and size raised in every member's superblock, and a
  superblock on the new device naming itself. No chunk is placed on it;
  the kernel allocates there as it needs the space. A device already in
  the filesystem, or one with no room past its first MiB, is refused.
