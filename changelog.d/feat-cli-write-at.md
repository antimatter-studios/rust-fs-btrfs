### Added

- `fs.btrfs TARGET write --at BYTES PATH` accepts a byte position inside a file and answers `not implemented` (exit 3) before reading stdin or opening the target, including at position zero. `--offset` remains the filesystem offset inside the target, and plain `write` keeps its existing behavior.
