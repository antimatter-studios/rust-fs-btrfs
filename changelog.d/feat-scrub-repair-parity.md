### Added

- **Scrub repairs a pool of several devices, and RAID5/6 data and parity
  (#350).** `scrub_repair` on a pool mounted with `mount_pool_rw` writes
  each bad copy on the member its stripe names, so a RAID1, RAID10 or
  RAID1C3/C4 pool is repaired from the copy on another member. On RAID5/6,
  where a block has one copy, a bad data sector or tree block is rebuilt
  from P, Q or both and written back, and a P or Q that is not what its
  data computes to is computed again and written. The scrub reports such
  damage as repairable. `tests/scrub_repair_kernel.rs` damages kernel-made
  RAID1, RAID5 and RAID6 pools, repairs them, and has the kernel's scrub,
  `btrfs check` and the kernel's own reads find them as the kernel made
  them; a repaired P or Q hashes to what the kernel's repairing scrub
  writes.
