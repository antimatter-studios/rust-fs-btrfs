### Added

- **A block group is relocated, and a single data group converted to DUP
  (#304: #397, #398, #399, #400).** `Filesystem::relocate_block_group`
  allocates a chunk of the group's profile -- its chunk item, a device
  extent per stripe, an empty block group item, its free-space records and
  the device's `bytes_used` -- in one committed transaction, moves the
  group's contents, then removes the old group in another. A data group's
  extents move at most 64 per committed transaction: each extent's bytes
  are read verified against their digests and written into every copy of
  the new chunk, and the batch's commit re-points every file item naming
  it (reflinks, clones within one file and the bookends of a partial
  overwrite included), moves its extent item and back references, files
  its checksums under the new sectors, and carries the free-space tree and
  both groups' `used` with it. Preallocated and `NODATASUM` extents move
  with the checksums they have and none are invented. A metadata group's
  tree blocks are copied out in one transaction
  (`Filesystem::copy_tree_blocks_out_of`), the group kept out of the tree
  block allocator while it empties.
- **DUP chunks are allocated on one device.** Both stripes are placed
  without overlapping, each with its device extent, and both charged to
  the device's bytes used.
- **A single data block group converts to DUP.**
  `relocate_block_group(start, Some(ChunkProfile::Dup))` moves the group
  into a new DUP chunk, every moved sector written to both copies.
- **A balance filter selects block groups.** `balance::Balance` takes the
  kernel's `kinds`, `usage`, `profiles` and `convert` filters, leaving a
  group already in the target profile where it is, as the kernel's soft
  mode does; `Balance::selects` answers for one group and
  `balance::profile_bits` gives a profile's chunk type bits.
- **What a relocation refuses**, before anything is written: a system group, any conversion but
  a single data group's to DUP, a group of any profile but single, a
  volume of more than one device, with no free-space tree or with a block
  group tree, a checksummed file's sector with no checksum, snapshot-shared
  data or tree blocks, mixed data and metadata groups, and a log tree
  awaiting replay. A batch that fails after an earlier one committed
  leaves every extent in exactly one of the two groups.
