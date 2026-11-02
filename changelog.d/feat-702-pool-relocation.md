### Added

- `Filesystem::relocate_block_group_off` moves a single or DUP block group in a pool onto the other members, never onto the device being emptied, with tree blocks kept off that device wherever another metadata group has room.

### Changed

- `relocate_block_group` works in a pool: the new chunk goes on the writable member with the most unallocated space, and a DUP group can stay DUP. A group with a stripe on a device the mount does not write, such as a seed, is refused.
