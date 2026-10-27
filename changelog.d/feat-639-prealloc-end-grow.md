### Added

- **A write spanning a kept preallocated item's end and the file's end is written (#639).** A copy-on-write write that starts in an item `fallocate --keep-size` preallocated past the end of a file and runs past that item, growing the file, is cut at the item's end: the part in the item is written where its extent lies and the item split, as the kernel does, and the rest is written as any growing write. It was refused.
