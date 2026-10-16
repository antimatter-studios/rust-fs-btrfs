### Added

- An inode-only log recovery candidate replaces metadata of existing empty regular files in the top-level subvolume and clears the log in the same transaction, judged against the kernel's own replay of the same crash image.
- The same recovery candidate also replays a same-length rewrite of an existing regular file's one uncompressed inline extent, installing the inode and extent items and clearing the log in one transaction; compressed, encrypted, otherwise encoded, external, resized and mixed logs are refused before any write.

### Fixed

- A test tier whose harness halt leaves the VM running retries the teardown, then destroys the VM, and fails only when the VM is still not confirmed stopped.
- The compressed-inline dirty-log fixture writes a payload zlib actually shrinks, and its producer requires the native dump to print a compressed inline extent; a focused workflow builds only the dirty-log fixtures.
