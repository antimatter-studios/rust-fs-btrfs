### Added

- **`fs.btrfs xattr PATH [NAME] [-o FILE]` (#416).** Without NAME it lists
  a path's extended attributes as JSON, each full name (with `name_hex` for
  one that is not UTF-8) and its value's size, sorted by name; with NAME it
  writes that value's raw bytes to stdout or to FILE. A symlink's own
  attributes are listed, and the image is never opened for writing.
