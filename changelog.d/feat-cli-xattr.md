### Added
- `fs.btrfs TARGET xattr PATH` lists extended attribute names and byte lengths as JSON or text; adding `NAME` reads its raw value to stdout or `-o FILE`, including inside subvolumes and snapshots. Non-UTF-8 names retain their exact bytes in `name_hex`.
