### Added

- `fs.btrfs TARGET export --recursive PATH DEST` copies a directory tree
  through rust-fs-core's shared tree transfer, entering subvolumes and
  snapshots as `ls --recursive` does. Every item is reported as completed,
  skipped, refused or failed, and a partial tree exits 1. Existing
  directories are merged into, `--max-depth` bounds the walk, and special
  files are skipped. A file whose data cannot be read fails alone and is
  never published. `--preserve=links` recreates symlinks without following
  them and keeps hard links, with a snapshot's files never linked to its
  source's; `--preserve=times` sets the stored access and modification
  times. `--verify` reads each staged file back before publication, for
  single files too. Checked against the guest kernel's own view of a tree 33
  directories deep (#541).

### Changed

- rust-fs-core is pinned at 0.3.15, the release carrying the shared tree
  transfer (#541).
