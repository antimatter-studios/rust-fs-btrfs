### Added

- **A member of a mirrored pool can be removed (#816).**
  `Filesystem::remove_device` relocates each RAID1, RAID1C3 and RAID1C4
  group on the device into a new group of the same profile, one copy on
  each of as many other members, those with the most unallocated space
  first. Balance and `relocate_block_group` move these groups too.
