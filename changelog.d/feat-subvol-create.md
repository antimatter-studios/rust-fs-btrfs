### Added

- **An empty subvolume can be created (#267).** `create_subvolume` makes
  one in a directory of the top-level subvolume, as one transaction: a new
  tree whose leaf holds the subvolume's top directory, its root item and
  both references in the root tree, its UUID in the UUID tree, and the
  entry naming it in the parent directory. A volume with quotas enabled,
  or whose top-level subvolume carries a `btrfs.*` property a new
  subvolume would inherit, is refused.
