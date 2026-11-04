## Writing

rename: Fallocate, dedupe
| Preallocating and punching a range of a file (`fallocate`, `punch_hole`, `fs_btrfs_fallocate`), leaving the items the kernel's `fallocate` and `fallocate -p` leave | Supported on a no-holes volume, in the top level and through a handle from `fs_btrfs_open_subvolume`. Refused: inline data, part of a sector in a `nodatacow` file's regular extent or in a preallocated extent another reference may read, space no data block group holds in one piece | Unreleased (#730, #771) | #713, #735 | `fallocate_kernel.rs`, `fallocate_edges_kernel.rs`, `capi_subvol_fallocate_contract.rs` |
| Deduplicating a range against another file (`dedupe_range`, `fs_btrfs_dedupe_range`) | Supported | Unreleased (#726) | #712 | `dedupe_kernel.rs` |
