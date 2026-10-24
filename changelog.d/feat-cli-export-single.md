### Added

- `fs.btrfs TARGET export PATH DEST [--replace]` copies a regular file through the shared bounded transfer, reports its byte count and SHA-256, and publishes a local destination only when complete. Subvolume and snapshot paths retain their own inode identity; `DEST -` streams raw bytes. Directories, symlinks and special files are refused.
