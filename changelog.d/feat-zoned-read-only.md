### Added
- Zoned volumes made on regular devices can be read using their fixed-offset superblocks; write mounts refuse zoned mode by name.

### Fixed
- A zoned superblock log at offset zero is refused before a potentially stale conventional superblock is selected.
