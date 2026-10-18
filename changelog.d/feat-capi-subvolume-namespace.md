### Changed

- **A C ABI subvolume handle writes its own tree.** On a read-write mount, `fs_btrfs_open_subvolume` returns a handle that creates, links, renames, unlinks, removes directories, truncates and writes inside that subvolume; its documentation no longer calls it read-only.
