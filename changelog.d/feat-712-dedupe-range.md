### Added

- **A range can be deduplicated against another file (#712).**
  `dedupe_range` and `fs_btrfs_dedupe_range` compare two ranges as the
  kernel's `FIDEDUPERANGE` does and, when they hold the same bytes, make the
  destination's range a clone of the source's in one committed transaction.
  A range must lie inside both files, only whole sectors are shared unless both
  ranges run to their files' ends, and the destination's change and
  modification times do not move.
