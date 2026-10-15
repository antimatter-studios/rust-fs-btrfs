### Fixed

- **A subvolume inside an ordinary directory is listed with that directory
  in its path (#362).** `Filesystem::subvolumes()` built each path from the
  chain of subvolume names alone, so a subvolume `nested` made in the
  directory `top` was listed as `nested`, a path that names nothing, where
  `btrfs subvolume list` prints `top/nested`. The path now runs through the
  directory each reference names, read from the parent subvolume's tree.
