## Writing

| A copy-on-write of a block a snapshot shares, whose children's references are items of their own | Supported: the new shared reference is filed as an item of its own too, once the extent has one or its record has no more room | Unreleased (#733) | | `subvol_snapshot_tall_kernel.rs` |
| A write through this crate into a snapshot whose tree is taller than one leaf, taken by this crate or by the kernel | Supported: the write copies the path it reaches, as the kernel's `update_ref_for_cow` does, whichever tree owns each block and whether or not it refers to its children by its address, and the source keeps its blocks and its bytes | Unreleased (#748) | #734 | `subvol_snapshot_tall_kernel.rs` |
