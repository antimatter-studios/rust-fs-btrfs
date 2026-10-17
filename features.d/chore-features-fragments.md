## Reading

| `..` in a path, including out of a subvolume into its parent | Supported | Unreleased (#271) | | `capi_subvol.rs` |
| A pool with members missing, mounted degraded (`mount_pool_degraded`); a loss some chunk cannot survive is refused at mount | Supported, read-only | Unreleased (#300) | | `pool_degraded.rs` |
| Profiles: raid5, raid6 | Supported: read, with any one element (RAID6: any two) rebuilt from parity, and written, every write a read-modify-write of the full stripes it touches with P and Q recomputed | Unreleased (#268, #299) | | `raid56_oracle.rs`, `raid56_write_kernel.rs` |
| Subvolumes and snapshots: listing, with id, path, parent, snapshot and read-only flags | Supported | 0.6.0; the path of one inside an ordinary directory Unreleased (#362) | | `subvol_oracle.rs` |
| A filesystem sprouted from a seed, read across the seed and the sprout | Supported, read-only | Unreleased (#270) | | `seed_oracle.rs` |
rename: A block group tree volume
| A block group tree volume (`-O block-group-tree`) | Supported, read-only; refused read-write, since block group usage is updated in the extent tree | Unreleased (#270) | | `block_group_tree_oracle.rs` |
rename: A volume with simple quotas
| A volume with simple quotas (`-O squota`) | Supported, read-only; refused read-write, since extents would be allocated without their owner references. Extents with owner references, which a 6.7+ kernel writes, have no fixture yet | Unreleased (#270) | #270 | `simple_quota_oracle.rs` |
| fs-verity files (`fsverity enable`): every block read checked against the file's Merkle tree and refused when it does not verify, as the kernel refuses it; the descriptor, digest (`fsverity measure`) and Merkle tree read back (`verity`, `verity_merkle_tree`) | Supported, read-only; a volume with verity files is refused read-write, since a data write would leave a sealed file's tree stale. A signed descriptor's signature is read but not checked | Unreleased (#356) | #270 | `verity_oracle.rs` |

## Checking

| Scrub, read-only: every copy of every allocated tree block and data sector checked, each bad copy reported with whether another verifies, and RAID5/6 parity recomputed for every full stripe holding data, a mismatching P or Q reported by device and offset | Supported | Unreleased (#268, #301) | | `scrub_oracle.rs`, `scrub_parity_oracle.rs` |
rename: Scrub repair
| Scrub repair (`scrub_repair`): a bad copy rewritten from a good one, on one device or on each member of a pool; on RAID5/6 a bad data sector or tree block rebuilt from parity, and a bad P or Q computed again from its data | Supported | Unreleased (#302, #358) | | `scrub_repair_kernel.rs` |

## Writing

| A volume with the v1 space cache (no free-space tree, `space_cache=v1`): read, checked and written | Supported: the cache is not kept; a commit leaves `cache_generation` behind, so the kernel discards it and rebuilds it at its next mount | Unreleased (#270) | | `space_cache_v1_kernel.rs` |
| Copy-on-write into a checksummed file, with the replaced extents' digests removed | Supported | Unreleased (#261) | | `cow_write_oracle.rs` |
| Copy-on-write into an extent a reflink or another item of the file shares: the copy drops this item's reference and the extent stays for the rest | Supported | Unreleased (#353) | | `cow_write_oracle.rs` |
| A volume whose log tree holds entries | Supported in every subvolume, all in one transaction: a read-write mount replays logged file data -- overwrites in place or inside a committed extent, appends, truncates, inline rewrites at any length or compressed -- with the extents' records and checksums, files and directories the log creates, and the names of directories the log speaks for, added, removed or renamed, deleting an inode that loses its last name; the log is read (`Filesystem::log`) and the volume opens read-only on its committed trees (`mount_ignoring_log`, like `ro,nologreplay`). Extended attributes are replayed as logged. Extended references and hard links are refused | Unreleased (#364, #375, #376, #377, #378, #379, #380, #403) | #266 | `log_tree_kernel.rs` |
| A pool of several devices, written (`mount_pool_rw`) | Supported, RAID5/6 included; a stale, read-only or missing member is refused | Unreleased (#298, #299) | | `pool_write_kernel.rs`, `raid56_write_kernel.rs` |
| Create, mkdir, symlink, link, unlink, rmdir; unlinking a file's last name releases its data extents, their back references and checksums, and drops its reference of an extent a reflink shares | Supported; data a snapshot may share is refused | Unreleased (#262) | | `namespace_kernel.rs` |
| Rename, replacing a name already there as POSIX does (`rename`, `fs_btrfs_rename`, `fs.btrfs mv`); a replaced file's last name releases its data | Supported; data a snapshot may share is refused | Unreleased (#262) | | `rename_kernel.rs` |
rename: Truncate
| Truncate (`truncate`, `fs_btrfs_truncate`, `fs.btrfs truncate`): shrinking releases what it cuts, or drops the file's reference of an extent a reflink shares; growing on a no-holes volume | Supported; an extent a snapshot may share is refused | Unreleased (#262) | | `truncate_kernel.rs` |
| Extended attributes, ACLs, mode, owner and times, written | Supported on the top-level subvolume; a full leaf a snapshot shares is refused | Unreleased (#263, #342) | | `attrs_kernel.rs`, `leaf_split_oracle.rs` |
rename: Compression on write
| Compression on write: zlib, LZO and zstd in the kernel's framing for each, for a file marked `chattr +c`, whose compression property names one, or on a mount set to compress (`set_compression`, `fs_btrfs_set_compression`, the kernel's `compress=`); stored uncompressed unless that saves a sector, a copy longer than one compression unit (128 KiB) stored as one extent per unit, and the first LZO or zstd extent sets the volume's feature flag for it | Supported | Unreleased (#265, #355, #357) | | `compress_write_kernel.rs` |
| Reflink and clone-range (`clone_range`, `fs_btrfs_clone_range`): a range of one regular file cloned into another, or into itself, on the top-level subvolume of a no-holes volume, sharing its extents | Partial: a destination range that already holds data, inline data, an extent whose references are kept as items of their own, and files that differ in checksumming are refused | Unreleased (#269) | #269 | `clone_kernel.rs` |
| Subvolumes and snapshots: making one read-only or writable again | Supported | Unreleased (#267) | | `subvol_readonly_kernel.rs` |
| Subvolumes and snapshots: choosing the default subvolume | Supported | Unreleased (#267) | | `subvol_default_kernel.rs` |
| Subvolumes and snapshots: the read-only flag and the default subvolume through the C ABI (`fs_btrfs_subvol_*`) | Supported | Unreleased (#267) | | `capi.rs`, `subvol_default_kernel.rs` |
| Subvolumes and snapshots: creating an empty subvolume in a directory of the top-level subvolume (`create_subvolume`) | Supported; a volume with quotas enabled, or whose top-level subvolume carries a `btrfs.*` property to inherit, is refused | Unreleased (#267) | | `subvol_create_kernel.rs` |
| Device add (`add_device`): a device joins a volume or a pool in one committed transaction, its `DEV_ITEM` in the chunk tree and every member's superblock counting it | Supported; a device already in the filesystem, or with no room past its first MiB, is refused | Unreleased (#303) | | `device_add_kernel.rs` |
| Device replace (`replace_device`): a new device takes a member's place whole, its devid, device UUID, recorded size and every device extent, and the old device's superblocks are wiped | Supported; the old device must be readable, and a smaller new device, or one already in the filesystem, is refused | Unreleased (#303) | | `device_replace_kernel.rs` |
| Device remove (`remove_device`): a device holding no chunk leaves in one committed transaction, its `DEV_ITEM` deleted, every other member's superblock counting it no more, and its own superblocks wiped | Supported for a device holding no chunk; one holding a chunk is refused until balance can relocate it, and so is the last device | Unreleased (#303) | #304 | `device_remove_kernel.rs` |
| Defragment (`defragment`, `defragment_to`): each run of a file's adjacent extents shorter than the target (32 MiB by default) rewritten as one, copy-on-write; a reflink's extents copied and the file's reference dropped, as the kernel does | Supported on the top-level subvolume; inline, preallocated and compressed extents and an extent the file references twice are left as they are; an extent a snapshot may share is refused | Unreleased (#305) | | `defrag_kernel.rs` |
| Trim: every free run of every block group handed to a caller's discard, on every copy | Supported; RAID5/6 groups refused | Unreleased (#305) | | `trim_oracle.rs` |
rename: Label, written to every superblock copy
| Label, written to every superblock copy; a primary recording another physical offset is refused before any write | Supported | 0.9.0 (#264); address refusal Unreleased (#370) | | `cli_label_kernel.rs`, `super_write` unit regression |
| Resize, library and C ABI (`resize`, `fs_btrfs_resize`): one device, grown into a larger device or shrunk when no chunk lies past the end | Supported; moving chunks and pools refused | Unreleased (#264) | | `resize_kernel.rs` |
rename: Resize, `fs.btrfs resize`
| Resize, `fs.btrfs resize`: a size, `+`/`-` a change, or `max`; an image file made longer before a grow, cut after a shrink (`--force`), and given its length back when the resize is refused | Supported; a chunk past the new end (a balance) and pools refused | Unreleased (#264) | | `cli_resize_kernel.rs`, `tests/cli/test-blocked.sh` |

## Send streams

| Reading a version 1 or 2 stream, every checksum verified | Supported | Unreleased (#273) | | `send_stream_kernel.rs` |
| Writing a full version-1 stream of a read-only subvolume | Supported | Unreleased (#273) | | `send_stream_kernel.rs` |
| Incremental send streams, written against a parent snapshot | Supported | Unreleased (#273) | | `send_stream_kernel.rs` |
| Version-2 send streams, with `FALLOCATE` and compressed extents passed through as `ENCODED_WRITE` | Supported | Unreleased (#273) | | `send_stream_kernel.rs` |
rename: Send clone sources
| Send clone sources: a reflinked run sent as a `CLONE` of what the receiver already has | Supported | Unreleased (#273) | | `send_stream_kernel.rs` |

## Interfaces

| C ABI: every read crosses into subvolumes | Supported | Unreleased (#271) | | `capi_subvol.rs` |
| C ABI: `fs_btrfs_mount_pool`, `fs_btrfs_open_subvolume` | Supported | Unreleased (#271) | | `capi_pool.rs` |
| C ABI and `fs.btrfs write` through the copy-on-write path | Supported | Unreleased (#274) | | `capi_write_kernel.rs`, `cli_write_kernel.rs` |
