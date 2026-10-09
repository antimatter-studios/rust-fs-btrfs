# Features

What this driver does today, what it refuses, and what is coming. **Every
pull request that adds, fixes, refuses or removes behaviour updates its row
here, in the same pull request** (AGENTS.md). The reasoning behind each change
is in [CHANGELOG.md](../CHANGELOG.md); how a transaction is put together is in
[cow-transaction.md](cow-transaction.md) and
[transaction-format.md](transaction-format.md).

**Since** is the release a row's current state shipped in, with the issue or
pull request the changelog cites for it. Work merged after the last release
is **Unreleased (#N)** until the next one. **Tracking** names the open issue
for anything not finished, and the open pull request when there is one.

States:

- **Supported**: works, and is checked against btrfs-progs or the Linux
  kernel in the harness VM.
- **Experimental**: works in every test, but is new.
- **Partial**: works for part of the case, and the row says which part.
- **Refused**: recognised and refused by name, rather than misread or
  approximated.
- **Not supported**: neither read nor refused by name.
- **Upcoming**: an open issue with a plan.

`tests/docs_describe_the_code.rs` reads three rows of this page by their
first words, so a row that falls behind the code it describes fails the
suite.

## Reading

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| Superblock (primary at 64 KiB) | Supported | 0.3.0 | | `oracle_vm_fixtures.rs` |
| Superblock mirrors: every copy read, the newest valid one used | Supported | 0.7.0 (#90) | | `superblock_mirrors.rs` |
| Metadata and data checksums: crc32c, xxhash64, sha256, blake2b | Supported | 0.3.0; data checksums of every width 0.7.0 (#102) | | `csum_oracle.rs`, `csum_width_populated.rs` |
| Chunk tree bootstrap and logical-to-physical mapping | Supported | 0.3.0 | | `bootstrap_chain.rs`, `chunk_tree_items.rs` |
| B-tree walk and keyed search, multi-level trees | Supported | 0.3.0 | | `btree_oracle.rs`, `fstree_oracle.rs` |
| Inodes, directory items, extent data, symlinks | Supported | 0.3.0 | | `fs_oracle.rs`, `kernel_readback.rs`, `readlink_kernel_oracle.rs` |
| Lookup by name key, without listing the directory | Supported | 0.7.0 | | `lookup_by_name_key.rs` |
| Mount without reading the whole filesystem tree | Supported | 0.7.0 | | `read_path_cost.rs` |
| Hard links across the `INODE_REF` to `INODE_EXTREF` spill | Supported | 0.8.0 (#196) | | `hard_link_extref.rs` |
| `..` in a path, including out of a subvolume into its parent | Supported | Unreleased (#271) | | `capi_subvol.rs` |
| Extended attributes, read | Supported | 0.7.0 | | `xattr_oracle.rs`, `getxattr_byte_names.rs` |
| POSIX ACLs, read | Supported | 0.8.0 (#197) | | `acl_oracle.rs` |
| Compression: zlib, LZO, zstd | Supported | 0.4.0 | | `compression_oracle.rs`, `compressed_short_decode.rs` |
| Profiles: single, dup, raid0, raid1, raid10 | Supported | 0.3.0; pools 0.6.0 | | `oracle_vm_fixtures.rs`, `pool_oracle.rs` |
| One bad copy of a DUP tree block | Supported: the other copy is read | 0.7.0 | | `dup_mirror_fallback.rs` |
| One device of a multi-device pool, opened alone | Refused | 0.6.0 | | `pool_oracle.rs` |
| Profiles: raid5, raid6 | Supported for reading, with any one element (RAID6: any two) rebuilt from parity; refused for writing | Unreleased (#268) | #299, #300 (PR #319) | `raid56_oracle.rs` |
| Mixed block groups (`mkfs.btrfs -M`) | Supported | 0.3.0 | | `oracle_vm_fixtures.rs` |
| Subvolumes and snapshots: listing, with id, path, parent, snapshot and read-only flags | Supported | 0.6.0 | | `subvol_oracle.rs` |
| Subvolumes and snapshots: reading inside one (`open_subvolume`) | Supported, read-only | 0.7.0 | | `subvol_oracle.rs`, `kernel_readback.rs` |
| A path that crosses into a subvolume (`resolve_path`) | Supported | 0.7.0 | | `subvol_oracle.rs`, `capi_subvol.rs` |
| An extent item whose window falls outside its extent | Refused | 0.7.0 | | `extent_window_read.rs`, `compressed_extent_window.rs` |
| An inode claiming an impossible size | Refused | 0.7.0 | | `read_file_huge_size.rs` |
| A metadata-only dump | Refused at every mount | 0.7.0 (#76) | | `super_flags_refusals.rs` |
| A seed device | Supported, read-only; refused read-write | 0.7.0 (#76) | PR #327 (reading a sprout) | `super_flags_refusals.rs` |
| A block group tree volume (`-O block-group-tree`) | Supported, read-only; refused read-write, since block group usage is updated in the extent tree | Unreleased (#270) | | `block_group_tree_oracle.rs` |
| A volume with simple quotas | Upcoming | | #270 (PR #310) | |
| Fuzzed decoders | Supported | 0.7.0 | | `fuzz_decoders.rs` |

## Checking

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| `fsck.btrfs`, check-only: tree-block checksums and keys, extents against their users, block groups, chunks against device extents, the free-space tree, the namespace | Supported | 0.9.0 (#260) | | `cli_fsck_oracle.rs` |
| Repair (`-y`, `-p`) | Refused (exit 16) | 0.9.0 (#260) | | `cli_fsck_oracle.rs` |
| Scrub | Upcoming | | #268, #301, #302 (PR #313, #321) | |

## Writing

Every write is a transaction committed as the kernel commits one, and every
transaction a test makes is judged by `btrfs check` and read back by the
Linux kernel in the guest.

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| Overwrite in place, `nodatacow` files | Supported | 0.5.0 | | `write_oracle.rs` |
| Overwrite in place on a `-d dup` volume, both copies | Supported | 0.7.0 | | `dup_data_writes.rs` |
| Whole transactions: tree edits, splits, relocation, commit order, backup roots | Supported | 0.6.0; backup roots 0.7.0 (#78) | | `transaction_oracle.rs`, `split_oracle.rs`, `commit_order.rs`, `backup_ring.rs` |
| Free-space tree and block-group `used` kept in step | Supported | 0.6.0; `used` 0.8.0 | | `free_space_oracle.rs`, `block_group_used.rs`, `free_space_straddle.rs` |
| A commit cut at any write | Supported: the volume is the old or the new generation | 0.8.0 | | `crash_consistency.rs` |
| Copy-on-write writes: overwrites inside a file's existing, unshared, unchecksummed, uncompressed extents | Partial | 0.8.1 (#61) | #261 (PR #318) | `cow_write_oracle.rs` |
| A write that grows a file or lands in a hole | Refused | 0.8.1 (#61) | #261, #262 (PR #323) | `cow_write_oracle.rs` |
| A write into a checksummed, inline, preallocated, compressed or shared extent | Refused | 0.7.0 (#74); copy-on-write 0.8.1 | #261 (PR #318) | `prealloc_extent_write.rs`, `snapshot_shared_write.rs`, `shared_tree_block_release.rs` |
| An extent item whose window falls outside its extent | Refused | 0.7.0 | | `extent_window_write.rs` |
| Moving a full-backref tree leaf | Partial: data back references are left inconsistent | | #287 (PR #297) | |
| A volume with a `compat_ro` feature the writer does not maintain | Refused for writing | 0.7.0 | | `compat_ro_write_refusal.rs` |
| A read-write mount whose newest superblock is not the primary | Refused | 0.7.0 (#90) | | `superblock_mirrors.rs` |
| A volume whose log tree holds entries | Partial: the log is read (`Filesystem::log`) and the volume opens read-only on its committed trees (`mount_ignoring_log`, like `ro,nologreplay`); every other mount refuses it, since the log is not replayed | Unreleased (#266) | #266 | `log_tree_kernel.rs` |
| A pool of several devices, written | Not supported | | #298 (PR #316) | |
| Create, mkdir, unlink, rmdir, rename, link, symlink, truncate | Not supported | | #262 (PR #311, #314, #323) | |
| Extended attributes, ACLs, mode, owner and times, written | Not supported | | #263 (PR #312) | |
| Compression on write | Not supported | | #265 (PR #325) | |
| Reflink, clone-range, fallocate, dedupe | Not supported | | #269 | |
| Subvolumes and snapshots: create, delete, read-only flag, default subvolume | Not supported | | #267 (PR #306, #309, #317) | |
| Device add, remove, replace; balance; defragment; trim | Not supported | | #268, #303, #304, #305 (PR #322) | |
| RAID5/6 writes | Not supported | | #299 | |
| Label, written to every superblock copy | Supported | 0.9.0 (#264) | | `cli_label_kernel.rs` |
| Resize | Not supported (`not implemented`, exit 3) | 0.8.0 | #264 (PR #326) | `tests/cli/test-blocked.sh` |

## Making a filesystem

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| `mkfs.btrfs`: one device, the standard formatter's defaults, node sizes 4 to 64 KiB, all four checksums, 128 MiB and up | Supported | 0.9.0 (#259) | | `cli_mkfs_kernel.rs` |
| Any other sector size or profile | Refused by name | 0.9.0 (#259) | | `cli_mkfs_kernel.rs` |

## Send streams

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| Reading a version 1 or 2 stream, every checksum verified | Supported | Unreleased (#273) | | `send_stream_kernel.rs` |
| Writing a full version-1 stream of a read-only subvolume | Supported | Unreleased (#273) | | `send_stream_kernel.rs` |
| Incremental send streams, written against a parent snapshot | Supported | Unreleased (#273) | | `send_stream_kernel.rs` |
| Version-2 writes, clone sources | Upcoming | | #273 (PR #320, #324) | |
| Applying a stream to an image (receive) | Not supported | | #273 | |

## Interfaces

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| C ABI: mount, read-write mount, volume info, stat, directory iterator, read, readlink, xattrs, in-place write | Supported | 0.3.0; writes 0.5.0 | | `capi.rs` |
| C ABI paths as bytes, not UTF-8 | Supported | 0.8.0 (#214) | | `capi.rs` |
| C ABI readlink contract shared with the family | Supported | 0.7.0 | | `readlink_kernel_oracle.rs` |
| C ABI: every read crosses into subvolumes | Supported | Unreleased (#271) | | `capi_subvol.rs` |
| C ABI: `fs_btrfs_mount_pool`, `fs_btrfs_open_subvolume` | Supported | Unreleased (#271) | | `capi_pool.rs` |
| C ABI and `fs.btrfs write` through the copy-on-write path | Supported | Unreleased (#274) | | `capi_write_kernel.rs`, `cli_write_kernel.rs` |
| `fs.btrfs` `ls`, `read`, `get`/`info` (`--features cli`) | Supported | 0.8.0 | | `cli_oracle.rs`, `tests/cli/test-read.sh` |
| `fs.btrfs write`: an existing `nodatacow` file, same length | Partial | 0.8.0 | #274 | `cli_write_kernel.rs` |
| `fs.btrfs mkdir` | Not supported (`not implemented`, exit 3) | 0.8.0 | #262 | `tests/cli/test-blocked.sh` |
| `fs.btrfs set label` | Supported | 0.9.0 (#264) | | `cli_label_kernel.rs` |
| `rust-fs-btrfs doctor`, man pages, shell completions | Supported | 0.8.0; pages 0.8.1 | | `cli_dispatch.rs`, `cli_docs.rs` |
