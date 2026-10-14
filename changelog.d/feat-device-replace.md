### Added

- **A device is replaced (#303).** `Filesystem::replace_device` gives a
  new device a member's place whole: every byte of every device extent
  at the offset it had, then the old device's superblock, so the new
  device is the same devid with the same device UUID and recorded size,
  as the kernel's own replace leaves it. No tree changes. The old
  device's superblocks are then wiped. A new device smaller than the old
  one, or already in the filesystem, is refused, and the old device must
  be readable.
