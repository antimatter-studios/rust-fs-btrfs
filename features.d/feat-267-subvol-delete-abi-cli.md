## Writing

| Subvolumes and snapshots: deleting one through the C ABI (`fs_btrfs_subvol_delete`) | Supported in a directory of the top-level subvolume; the default subvolume, one holding another, a handle on another subvolume and a volume with quotas enabled are refused with ENOTSUP | Unreleased (#267) | | `capi_subvol.rs` |

## Interfaces

| `fs.btrfs subvolume delete` | Supported in a directory of the top-level subvolume; a missing name, one that is no subvolume or a missing parent answers with exit 4, what the library refuses with exit 3 | Unreleased (#267) | | `cli_subvolume.rs` |
