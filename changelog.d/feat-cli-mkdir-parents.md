### Added

- `fs.btrfs mkdir --parents PATH` (`-p`) makes every missing directory on the
  way to PATH, mode 0755 (`--mode` is PATH's own), and reports
  `{"path": ..., "created": [...]}`, listing only the directories made. A
  chain already there, through a subvolume included, is success with nothing
  written. A file, a symlink or a name too long anywhere on the way, and an
  empty path, exit 1 with the reason the kernel's `mkdir -p` gives, and a
  directory inside a subvolume other than the top level exits 3; each is
  refused before the image is opened for writing, so no partial chain is
  left. A symlink is never followed (#414).
