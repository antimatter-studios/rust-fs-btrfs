### Added

- **Names are made inside a subvolume other than the top level (#517).**
  A handle from `open_subvolume` on a writable mount was read-only, so
  nothing could be created inside any subvolume but the top level. Its
  `create`, `mkdir` and `symlink` now commit against that subvolume's own
  tree, and the handle stays on the subvolume across each commit; every
  other write still refuses it as read-only. `tests/subvol_namespace_kernel.rs`
  has the kernel make a subvolume holding a file, makes a directory, a
  file and a symbolic link inside it, and has `btrfs check` call the
  volume clean and the kernel list the new names with the top level
  unchanged. Links, removals, renames and truncation inside a subvolume
  are still refused (#518, #519).
