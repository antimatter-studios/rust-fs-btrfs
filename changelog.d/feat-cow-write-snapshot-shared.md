### Added

- A copy-on-write write into an extent a snapshot shares copies it: the top-level subvolume's item is pointed at the copy, and the old extent keeps the snapshot's reference and loses only the top level's, instead of refusing the write. A file written before a read-only or writable snapshot can now be written again.
