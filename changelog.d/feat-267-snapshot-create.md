### Added

- **A subvolume can be snapshotted (#267).** `create_snapshot` takes a
  snapshot of the top-level subvolume or any other, writable or
  read-only, into a directory of the top-level subvolume as one
  transaction, the way `btrfs subvolume snapshot` does: the new tree's
  root is a copy of its source's, each data extent the source refers to
  gains a reference from the snapshot, and the snapshot's root item names
  its source's UUID as its parent. A source whose tree is taller than one
  leaf is refused.
- **A subvolume's UUIDs can be read (#267).** `subvolume_uuids` returns a
  subvolume's own UUID and the UUID of the subvolume it is a snapshot of.
