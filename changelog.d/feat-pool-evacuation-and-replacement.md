### Changed

- **A device holding chunks can be removed (#816).**
  `Filesystem::remove_device` first relocates every single and DUP data
  and metadata group on the device onto the other members, then drops the
  device. A device holding a system, RAID0, RAID10, RAID5 or RAID6 group,
  or one whose groups the other members have no room for, is refused
  before anything is written.
