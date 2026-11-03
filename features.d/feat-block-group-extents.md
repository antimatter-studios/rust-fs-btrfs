## Reading

| A data block group's extents, with every file item naming each | Experimental; refused on a volume with snapshots or shared data references | Unreleased (#396) | | `block_group_extents_kernel.rs` |

## Writing

| Device remove of a device holding chunks; balance | Partial: balance and its relocations and conversions, through the library, the C ABI and `fs.btrfs balance` (their own rows); removing a device that holds chunks is not supported | Unreleased (#304) | #268, #303 | `relocate_data_group_kernel.rs`, `relocate_convert_dup_kernel.rs`, `cli_balance_kernel.rs` |
