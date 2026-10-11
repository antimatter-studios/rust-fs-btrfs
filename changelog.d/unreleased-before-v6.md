### Added

- **`docs/features.md`, a features page kept current by every pull request.**
  Each feature's state, the release it shipped in, its tracking issue and the
  test that checks it. The README's status table is a short summary pointing
  to it, and `tests/docs_describe_the_code.rs` reads its rows from the page.
- **A reflink copy is sent as a clone (#273).** A run of a data extent that a
  file the receiver already has holds -- one sent earlier in the stream, or,
  in an incremental stream, one of the parent's files -- is sent as a `CLONE`
  of it instead of its bytes, when the receiver's clone can take the range.
  `tests/send_stream_kernel.rs` has `btrfs receive` turn both kinds into
  shared extents.
- **A checksummed file can be written copy-on-write.** Each copy gets a
  digest for every sector in the volume's checksum algorithm, and the
  digests of the extents it replaces are removed — an `EXTENT_CSUM` item the
  kernel shared between neighbouring extents is cut down to the neighbours'
  sectors rather than dropped. Most files are checksummed, so most files can
  now be overwritten; growing a file, filling a hole, and inline,
  preallocated, compressed or shared extents are still refused (#261).
- **A scrub checks RAID5/6 parity (#301).** `Filesystem::scrub` recomputes P,
  and Q, for every full stripe holding allocated bytes and compares them with
  what is on disk, in the sectors some data element holds, as the kernel's
  scrub does. A mismatch is a `ScrubTarget::Parity` error naming the element's
  device and offset (`ScrubError::device`), repairable when its data verifies;
  `ScrubReport::parity_stripes` counts the full stripes checked. Checked
  against RAID5 and RAID6 pools with P and Q damaged on purpose, located from
  the chunk tree as btrfs-progs prints it, and the parity the kernel's own
  scrub wrote back over them.
- **A file can be truncated.** `Filesystem::truncate` commits one transaction:
  shrinking deletes the extent items wholly past the new end and releases
  each extent one of them was the only reference to — its record, its free
  space, its block group's `used` and its digests — and references less of
  the extent it cuts; growing moves only the size, on a no-holes volume. The
  inode's `nbytes` is recounted as `btrfs check` counts it. The C ABI gains
  `fs_btrfs_truncate` and `fs.btrfs` gains `truncate`. An extent a snapshot
  or reflink shares is refused (#262).
- **A transaction can release a data extent outright.** Beside the extents a
  copy-on-write write replaces, a change can give up extents with no copy
  taking their place, with the same refusal of any extent held by more than
  the item being removed.
- **A file that asks for compression is written compressed.** A
  copy-on-write copy of up to 128 KiB in a file marked `chattr +c`, or whose
  compression property names zlib, is stored as one zlib stream padded to a
  sector when that saves a sector, with its extent record, free space, block
  group `used` and digests counting the on-disk length. A compressed extent
  can now be overwritten too: the copy is made from its decoded bytes. zstd
  and LZO are not written; a file asking for them is stored uncompressed
  (#265).
- **A subvolume can be made read-only or writable again (#267, first
  slice).** `Filesystem::set_subvolume_read_only` sets or clears bit 0 of the
  subvolume's `ROOT_ITEM` flags and commits it as one copy-on-write
  transaction, as `btrfs property set <subvol> ro` does. A received subvolume is
  refused when made writable, because its received UUID would have to be
  cleared. `tests/subvol_readonly_kernel.rs` has `btrfs check` find the volume
  clean, `btrfs property get` report the new flags, and the kernel refuse a
  write into the newly read-only subvolume and accept one into the newly
  writable snapshot.
- **A filesystem can be resized.** `Filesystem::resize` grows a single-device
  filesystem into a device the caller has already made larger, or shrinks it
  when no chunk lies past the new end, as one transaction: the chunk tree's
  device item, the superblock's size and its copy of the device item. The
  planner now moves chunk tree blocks, within SYSTEM block groups, and the
  commit moves the superblock's chunk root. The C ABI gains
  `fs_btrfs_resize`; `fs.btrfs resize` still answers not implemented. Moving
  chunks (a balance) and pools of several devices are refused (#264).
- **RAID5 and RAID6 pools are written (#299).** Every write into a parity
  chunk, a file's extents and the commit's tree blocks alike, is a
  read-modify-write of each full stripe it touches: the data elements read,
  the new bytes applied, P and Q recomputed, and the changed elements written
  to the devices their stripes name. `mount_pool_rw` no longer refuses them.
  Checked on RAID5 and RAID6 pools the kernel made: after writes into one
  element, across element and full-stripe boundaries and over whole full
  stripes, `btrfs check` finds each clean, the kernel reads every file back,
  and this crate's scrub finds the parity in agreement with the data.
- **Extended attributes, ACLs, mode, owner and times can be written.**
  `Filesystem::set_xattr`, `remove_xattr`, `set_mode`, `set_owner` and
  `set_times` each commit one transaction on the top-level subvolume and move
  the inode's change time, as Linux does; an ACL is the
  `system.posix_acl_access` or `system.posix_acl_default` attribute in the
  kernel's encoding. The C ABI gains `fs_btrfs_setxattr`,
  `fs_btrfs_removexattr`, `fs_btrfs_chmod`, `fs_btrfs_chown` and
  `fs_btrfs_utimens` (with `FS_BTRFS_UTIME_OMIT`). A leaf with no room for a
  new attribute item is refused with nothing written (#263).
- **The C ABI mounts a pool and opens a subvolume.** `fs_btrfs_mount_pool`
  takes every device of a multi-device filesystem, in any order, and
  `fs_btrfs_open_subvolume` gives a read-only handle rooted at one subvolume
  or snapshot by its id (#271).
- **Names can be created and removed.** `Filesystem::create`, `mkdir`,
  `symlink`, `link`, `unlink` and `rmdir` each commit one transaction on the
  top-level subvolume: the inode, its reference, both directory entries and
  the parent's size and times, through the same planner and commit as a
  copy-on-write write. The C ABI gains `fs_btrfs_create`, `fs_btrfs_mkdir`,
  `fs_btrfs_symlink`, `fs_btrfs_link`, `fs_btrfs_unlink` and `fs_btrfs_rmdir`
  (EEXIST, ENOTEMPTY, EISDIR and ENOTDIR where POSIX says), and `fs.btrfs`
  gains `create`, `rm`, `rmdir` and `ln [-s]`, and its `mkdir` works. A
  directory whose leaf has no room, and the last name of a file still holding
  data extents, are refused with nothing written; `rename` and `truncate` are
  still to come (#262).
- **A transaction can put and delete fs tree items.** The planner renders item
  edits into the leaves they land in, and a leaf whose first key changes has
  its parent's key follow it.
- **Send streams are read and written (#273).** `send::parse_send_stream`
  reads a version 1 or 2 stream and verifies every command's checksum, and
  `Filesystem::send_subvolume` writes a full version-1 stream of a read-only
  subvolume. `tests/send_stream_kernel.rs` has the kernel judge both: a stream
  `btrfs send` wrote parses into the commands `btrfs receive --dump` lists and
  replays into the snapshot the kernel shows, and `btrfs receive` rebuilds the
  same snapshot from a stream this crate wrote.
- **The default subvolume can be chosen (#267).**
  `Filesystem::set_default_subvolume` repoints the root tree's `default`
  entry and, for any subvolume but the top level, sets the `DEFAULT_SUBVOL`
  incompatible feature in the same commit, as `btrfs subvolume set-default`
  does. `tests/subvol_default_kernel.rs` has `btrfs check` find the volume
  clean, `btrfs subvolume get-default` name the new default, and a plain kernel
  mount show its contents.
- **The C ABI reads and changes a subvolume's read-only flag and the default
  subvolume (#267).** `fs_btrfs_subvol_is_readonly`,
  `fs_btrfs_subvol_set_readonly`, `fs_btrfs_subvol_is_default` and
  `fs_btrfs_subvol_set_default` name a subvolume by the path of its top
  directory, as a mount shows it, and fail with ENOENT for any other path and
  EROFS on a handle not opened for writing. Behind them,
  `Filesystem::subvolume_at` finds a subvolume by that path and
  `Filesystem::default_subvolume` reads the default back;
  `tests/subvol_default_kernel.rs` holds both to what the kernel says.
- **Incremental send streams are written (#273).**
  `Filesystem::send_subvolume_incremental` writes what changed between two
  read-only snapshots, as `btrfs send -p` does: names taken away, moved and
  made through orphan names in the top directory so no rename lands on a name
  in use or inside itself, only the ranges of a kept file whose extent items
  read from somewhere new, and changed extended attributes, owners, modes and
  times. `tests/send_stream_kernel.rs` has the guest's `btrfs receive` apply
  it on top of the parent and the kernel show the child exactly, and parses
  the kernel's own incremental stream against `btrfs receive --dump`.
- **A trim.** `Filesystem::trim_ranges` lists every free run of every block
  group, from the free-space tree (or the extent tree's gaps without one),
  mapped to the device range of each copy, and `Filesystem::trim` hands each to
  a discard the caller supplies, on a read-write mount, changing nothing on the
  filesystem. Checked against the free runs `btrfs inspect-internal dump-tree`
  lists and the copies its chunk tree places (#305). RAID5/6 groups are
  refused.
- **A name can be moved.** `Filesystem::rename` commits one transaction that
  removes the old entries and reference and files new ones under a fresh
  index in the destination, replacing a name already there as POSIX `rename`
  does. The C ABI gains `fs_btrfs_rename` (ENOTDIR, EISDIR, ENOTEMPTY and
  EINVAL where POSIX says), and `fs.btrfs` gains `mv` (#262).
- **A read-only scrub.** `Filesystem::scrub` reads every copy of every tree
  block and data sector the extent tree says is allocated, checks each against
  its checksum (and a tree block against its own address and filesystem), and
  reports every bad copy with whether another copy of it verifies. Nothing is
  repaired. Checked against the kernel's `btrfs scrub start -r` on a DUP volume
  with three copies damaged on purpose (#268).
- **Version-2 send streams are written, with compressed data passed through
  (#273).** `Filesystem::send_subvolume_with` and
  `Filesystem::send_subvolume_incremental_with` take `send::SendOptions`: a
  version-2 stream keeps a preallocated range as a `FALLOCATE`, and with
  `compressed_data` sends each zlib, LZO or zstd extent as an `ENCODED_WRITE`
  of its bytes as they are on disk, as `btrfs send --proto 2
  --compressed-data` does. `tests/send_stream_kernel.rs` has every encoded
  write match the kernel's own stream byte for byte, and `btrfs receive`
  rebuild the snapshot with its extents still compressed and its preallocated
  range still allocated.
- **A volume with simple quotas is read.** The `simple_quota` incompat bit
  refused the mount; it moves no file's bytes, so a read-only mount now
  accepts it, checked against a volume btrfs-progs 7.1 formatted with
  `-O squota --rootdir`, which `btrfs check` finds clean. An extent carrying an
  owner reference, which only a kernel of 6.7 or later writes, is not covered
  yet. A read-write mount is refused, naming simple quotas (#270).
- **A pool of several devices can be written.** `Filesystem::mount_pool_rw`
  opens every member read-write: each copy of a mirrored chunk is written on
  the device its stripe names, and a commit flushes every member and writes
  each one's superblocks, keeping its own `dev_item`. A member that missed a
  commit, or whose primary superblock is not its newest, is refused, as is a
  read-only member and an incomplete set. RAID5/6 stays refused for writing.
  Checked by writing a two-device RAID1 pool and having `btrfs check` and the
  kernel read it back (#298).
- **`send::StreamError`**, its own error type. A malformed stream is bytes
  handed in, not a damaged volume, so it is kept out of the crate's `Error`.
- **An unreplayed log tree can be read.** `Filesystem::log` returns every item
  of every subvolume's log tree, with the logged inodes, names and file
  contents, and `Filesystem::mount_ignoring_log` opens such a volume read-only
  on its committed trees, like the kernel's `ro,nologreplay`. Every other mount
  still refuses a non-empty log; replaying it is the rest of #266.
- **A filesystem sprouted from a seed is read.** A seed device's own
  superblock names the seed's fsid, not the sprouted filesystem's, and
  `mount_pool` refused it as a device of another filesystem. A seeding device
  given with a sprout is now accepted once the sprout's chunk tree holds a
  `DEV_ITEM` for it with that fsid and device uuid, and refused otherwise.
  Checked against a seed and a sprout the kernel made, every file of each read
  as the kernel wrote it (#270). Writing a seed stays refused.
- **A scrub repairs a bad copy from a good one.** `Filesystem::scrub_repair`,
  on a read-write mount, scrubs and then writes a copy that verifies over every
  copy that does not, with no transaction, since the logical contents do not
  change; a bad copy with no good twin is listed as unrepairable. After it, the
  kernel's scrub counts no error and `btrfs check` finds the volume clean
  (#302). A pool of several devices is not repaired yet.
- **RAID5 and RAID6 pools are read.** The `raid56` feature no longer refuses
  the mount: data elements are found round the rotating parity, and an element
  whose checksum fails is rebuilt from the rest of its full stripe — from P,
  from Q, or for RAID6 together with a second lost element from both. Checked
  against three- and four-device pools the kernel wrote, whole, with any one
  device returning garbage, and for RAID6 with any two (#268). Writing a
  parity pool is refused.
- **A data sector that fails its checksum is read from the next copy.** The
  second copy of a DUP or RAID1 chunk was used for tree blocks only; a file's
  data now falls back the same way before it is refused (#268).
- **A pool with members missing mounts degraded.**
  `Filesystem::mount_pool_degraded` accepts fewer devices than the pool spans,
  as the kernel's `-o degraded` does, and reads every element on a missing
  member the other way its chunk offers: the other copy, or a rebuild from
  parity. A loss some chunk cannot survive (any member of a single, RAID0 or
  DUP chunk; two of RAID5; three of RAID6) is refused at mount, naming the
  chunk. Checked against the kernel-written RAID5 pool without each member,
  RAID6 without every pair and RAID1 without either (#300).

### Changed

- **`fs.btrfs write` and `fs_btrfs_write_file` write ordinary copy-on-write
  files.** Both went through the in-place path, which writes only NODATACOW
  files, so the library's copy-on-write writes were unreachable from the tool
  and the C ABI. They now go through `Filesystem::write`: a NODATACOW file is
  still overwritten in place, and any other unchecksummed file is written into
  new extents and committed as one transaction. What that path cannot write yet
  is refused with the case named and #261 cited (#274).

### Fixed

- **Guest scripts read every pipe to the end.** Under `pipefail`, a `head` or
  an `awk` that exits early could kill the writer before it with SIGPIPE and
  fail a kernel-oracle test at random (exit 141, #279). Six such readers in
  four tests now read every line, and a guard refuses new ones.
- **The block-group used-count test picks a block whose move really crosses
  groups.** It asks the allocator where its next block goes and tries every
  tree block outside that group, of every tree a plan can move, instead of the
  first 300 fs and csum leaves; when the kernel's layout offers none, the
  failure names that layout (#279, #280).
- **The block-group used-count test asks the kernel for another volume on a
  layout miss (#343).** About one kernel-made volume in thirty held no block
  whose move crosses groups cleanly, every candidate's extent item landing in
  a full extent-tree leaf (#342), and the test failed the run. It now builds a
  fresh volume, up to three times, and fails only when every one misses,
  naming each layout.
- **Names, attributes and subvolume flags change on a pool mounted read-write
  (#336).** `mkdir`, `create`, `set_mode` and the other item-edit changes,
  and setting a subvolume read-only or the default, reopened one device after
  their commit and so returned an error on a pool, having committed. They
  now reopen every member, as the copy-on-write write does.
- **A path with `..` in it resolves.** `lookup_path` and `resolve_path`
  refused it; `..` is now the directory the walk came from, the top's parent
  is the top, and climbing out of a subvolume lands in its parent tree, as the
  kernel reads the same paths (#271).
- **The C ABI reads inside subvolumes.** `fs_btrfs_stat`, `fs_btrfs_read_file`,
  `fs_btrfs_readlink`, `fs_btrfs_listxattr` and `fs_btrfs_getxattr` stopped at
  a subvolume's boundary while `fs_btrfs_dir_open` listed what was beyond it;
  every reading call now crosses (#271).
- **A transient HTTP 5xx from the chore release download no longer fails a CI
  job.** `scripts/ci-install-chore.sh` retries both downloads up to five times
  on any error; the checksum check still guards what was fetched.
- **Moving a tree block that a balance flagged `FULL_BACKREF` keeps its
  references true.** The data extents under such a leaf, and the children of
  such a node, are recorded as referred to by the block's address. The move
  used to leave them naming the old block, and `btrfs check` reported a
  referencer count mismatch for every extent. The copy is now an ordinary
  block of its tree, as the kernel's copy-on-write makes it: its record drops
  the flag, and each of those references becomes the keyed one, by tree or by
  inode and offset, with the same count. The shapes this does not write yet
  are refused by name: a data extent the leaf names at several inodes or
  offsets, one with other keyed data references, and one whose references
  would no longer fit inline (#287).
- **A block group tree volume's block groups are found.** With the
  `block_group_tree` feature the block group items live in tree 11, not the
  extent tree, and `block_groups` looked only in the extent tree: the list was
  empty and `fsck.btrfs` reported a volume `btrfs check` finds clean. Checked
  against `btrfs inspect-internal dump-tree` on a volume the kernel populated
  (#270).
