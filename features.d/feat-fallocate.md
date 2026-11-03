## Writing

rename: Fallocate, dedupe
| Preallocating and punching a range of a file (`fallocate`, `punch_hole`, `fs_btrfs_fallocate`), leaving the items the kernel's `fallocate` and `fallocate -p` leave | Supported on the top-level subvolume of a no-holes volume. Refused: inline data, part of a sector in a `nodatacow` file's regular extent or in a preallocated extent another reference may read, space no data block group holds in one piece | Unreleased (#730) | #713 | `fallocate_kernel.rs` |
| Deduplicating a range against another file (`dedupe_range`, `fs_btrfs_dedupe_range`) | Supported | Unreleased (#726) | #712 | `dedupe_kernel.rs` |
