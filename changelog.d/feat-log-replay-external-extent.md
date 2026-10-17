### Added

- **A log that replaces an existing file's extent is replayed** (#375). An
  overwrite made durable by `fsync` alone leaves the file's new extent in
  the log, unrecorded by the extent tree. A read-write mount now records
  that extent, drops the replaced extent's reference (releasing it when that
  was the last), and moves the checksums, after checking that the logged
  digests match the bytes on disk, as the kernel's replay does.
