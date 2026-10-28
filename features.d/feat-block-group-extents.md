## Reading

| A data block group's extents, with every file item naming each | Experimental; refused on a volume with snapshots or shared data references | Unreleased (#396) | #304 | `block_group_extents_kernel.rs` |

## Writing

| Device remove of a device holding chunks; balance | Partial: balance, its relocations and conversions, through the library, the C ABI and `fs.btrfs balance` (their own rows); removing a device that holds chunks is not supported, since a new chunk's stripes are placed on one device only | Unreleased (#304, #402) | #268, #303 | `relocate_data_group_kernel.rs`, `cli_balance_kernel.rs` |
