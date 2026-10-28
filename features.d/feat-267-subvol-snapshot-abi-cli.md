## Writing

| Subvolumes and snapshots: snapshotting one through the C ABI (`fs_btrfs_subvol_snapshot`) | Supported into a directory of the top-level subvolume, writable or read-only; a source whose tree is taller than one leaf, a handle on another subvolume and a volume with quotas enabled are refused with ENOTSUP | Unreleased (#267) | #700 | `capi_subvol.rs`, `capi_subvol_snapshot_kernel.rs` |

## Interfaces

| `fs.btrfs subvolume snapshot` | Supported into a directory of the top-level subvolume, `--read-only` as `-r`; a source that is no subvolume, a name already there or a missing parent answers with exit 4, what the library refuses with exit 3 | Unreleased (#267) | #700 | `cli_subvolume.rs` |
