### Added

- **A subvolume of any height is snapshotted (#700).** `create_snapshot`
  copies a root node as it copies a root leaf, and each child gains a
  reference from the snapshot, as the kernel's `btrfs_inc_ref` gives one.
  The top level's own blocks that the snapshot's entries land in are copied
  as blocks the snapshot shares.
- **A snapshot files references as items of their own when the kernel
  would (#700).** An extent or tree block that already keeps a reference
  as an item, or whose record would grow past what stays inline, gains the
  snapshot's as an item too, and its record counts it.

### Fixed

- **Copying a block a snapshot shares no longer refuses an extent with
  keyed references (#700).** The shared reference each child gains is filed
  as an item of its own under the same rule.
