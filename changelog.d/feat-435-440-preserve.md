### Added

- **`fs.btrfs import` and `export` keep links, times and attributes (#435, #440).**
  `import --recursive --preserve=times,links,xattrs` keeps hard links, symbolic
  links, access and modification times and extended attributes, and
  `export --recursive --preserve=xattrs` carries extended attributes out.

### Changed

- **A partial tree export exits 5.** With rust-fs-core 0.4.3, a recursive export
  that published what it could and reported the rest as refused or failed exits
  with the partial status instead of 1.
