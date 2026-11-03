## Writing

| A copy-on-write of a block a snapshot shares, whose children's references are items of their own | Supported: the new shared reference is filed as an item of its own too, once the extent has one or its record has no more room | Unreleased (#733) | | `subvol_snapshot_tall_kernel.rs` |
| A write through this crate into a snapshot whose tree is taller than one leaf | Refused, nothing written; the source takes one, and the kernel takes either | Unreleased (#733) | #734 | `subvol_snapshot_contract.rs` |
