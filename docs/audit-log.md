# Write-path audit log

Every pass over this driver's write paths asks one question:

> Is there an edit that can be applied to a filesystem the driver has **misread**?

The question is not about a missing bounds check. It's about a value read one way and then used another, on an ordinary filesystem. That's the shape of every corruption defect found in this family of drivers so far. The same log, in the same format, is kept in `rust-fs-ext4`, `rust-fs-xfs`, `rust-fs-btrfs` and `rust-fs-ntfs`.

Each pass is one entry, newest first. An entry names:
- the commit it read;
- every module it looked at, with the result, "none found" included;
- each finding with its issue;
- what it didn't reach, so the next pass knows where to start.

## 2026-09-17: write, allocation, transaction and commit paths

Read at `ece3fa9` (#63).

| Module | Result |
|---|---|
| `src/write.rs` (in-place nodatacow write) | #173 |
| `src/transaction.rs`: `next_free_block` | #175 |
| `src/block_group.rs`: free extents, `find_metadata_block` | #175 |
| `src/transaction.rs`: `apply_free_space` | #177 |
| `src/transaction.rs`: `plan_transaction`, `apply_records` | #178 |
| `src/transaction.rs`: `render_plan` | none found |
| `src/leaf_edit.rs` | none found |
| `src/extent_write.rs` | none found |
| `src/commit.rs`, `src/super_write.rs` | none found |

**Findings:**
- **#173, fixed in #174.** `write_at` took an extent item's `refs == 1` to mean one reader. A snapshot of a node-rooted tree leaves data extents at 1, so the write changed the snapshot's copy. Reproduced on a kernel-made snapshot.
- **#175, fixed in #176.** The allocator took the extent tree's gaps as free, including the `stripe_len` rows holding superblock copies. On a 256 MiB volume it handed out the block the commit's second superblock is written over. Reproduced locally.
- **#177, from reading the code.** The free-space-tree rewrite assumes a block group's records sit in one leaf.
- **#178, from reading the code.** A transaction releases a tree block by deleting its extent record, whatever its reference count. That's reachable through the public planner on a snapshotted tree.

**Looked at and not a finding:**
- The commit builds its superblock from the raw primary, but a writable mount is refused unless copy 0 is the one chosen (#146).
- `render_plan` drops leaf items whose data can't be located, but leaves are validated when read.
- `leaf_edit::insert_or_split` splits by count, not bytes. It has no production caller, and `build_leaf` refuses an oversized leaf.
- `window_inside_extent` bounds a write by the extent tree's own length (#89).

**Not reached:** `src/capi.rs`'s write entry points beyond what they call, `src/xattr.rs` and `src/tree_write.rs`'s node builder.
