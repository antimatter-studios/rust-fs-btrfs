### Added

- **A subvolume can be deleted (#267).** `delete_subvolume` removes one
  from a directory of the top-level subvolume as one transaction, the way
  `btrfs subvolume delete` does: the parent's entry, the subvolume's
  references and its UUID go, its root item is marked dead, and an orphan
  item leaves its tree for the kernel's cleaner to drop on the next
  read-write mount. The default subvolume, and one that holds another
  subvolume, are refused.

### Changed

- **A deleted subvolume is not listed (#267).** `subvolumes` leaves out a
  tree whose root item no directory refers to any more, as
  `btrfs subvolume list` does.
