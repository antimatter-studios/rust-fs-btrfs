### Fixed

- **A seed and its sprout given for writing are refused by name (#561).**
  `mount_pool_rw` refused the two devices of a sprouted filesystem as
  `these devices belong to different filesystems`, though they are one
  filesystem and `mount_pool` reads them. The refusal now says a device is a
  seed and that a filesystem sprouted from a seed can be read but not yet
  written.
