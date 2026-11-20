### Deprecated

- **The read-only openers `Filesystem::mount`, `Filesystem::mount_with_cache` and `Filesystem::mount_ignoring_log`.** Open a volume through rust-fs-core's `OpenVolume` instead: `layers::BtrfsVolume::open` for the validating mount, and `layers::BtrfsVolume::open_for_salvage` for a volume whose log is not replayed. `BtrfsVolume::filesystem` lends the `Filesystem` for what the layers do not cover. The deprecated openers are unchanged wrappers over the same open and stay for one release. The rest of `Filesystem`, the write path and the C ABI are not deprecated.

### Added

- **A salvage open browses a volume whose log was never replayed.** When the validating mount refuses only an unreplayed log, `BtrfsVolume::open_for_salvage` opens the committed trees the way the kernel's `ro,nologreplay` does, and the file layer can browse them. The health verdict still reports the log.
