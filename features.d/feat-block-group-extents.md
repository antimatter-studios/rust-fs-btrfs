## Reading

| A data block group's extents, with every file item naming each | Experimental; refused on a volume with snapshots or shared data references | Unreleased (#396) | #304 | `block_group_extents_kernel.rs` |

## Writing

| Device remove of a device holding chunks; balance | Partial: a block group is relocated and a single data group converted to DUP through the library (their own rows); snapshot-shared data, other conversions, the C ABI and a CLI verb, and removing a device that holds chunks are not supported | Unreleased (#304) | #268, #303, #304: #401, #402 | `relocate_data_group_kernel.rs`, `relocate_convert_dup_kernel.rs` |
