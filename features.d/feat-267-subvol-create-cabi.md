## Writing

| Subvolumes and snapshots: creating an empty subvolume through the C ABI (`fs_btrfs_subvol_create`) | Supported in a directory of the top-level subvolume; a handle on another subvolume, a volume with quotas enabled, or a top level carrying a `btrfs.*` property to inherit is refused | Unreleased (#267) | | `capi_subvol.rs`, `capi_subvol_create_kernel.rs` |
