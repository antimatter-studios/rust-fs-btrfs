## Writing

| A metadata block group relocated into a new chunk | Experimental; single profile, one device, free-space tree; shared tree blocks, mixed groups and an unreplayed log are refused | Unreleased (#583) | #580, #575 | `relocate_metadata_group_kernel.rs` |
| System block group relocation | Refused by name before any write because the superblock chunk array also names the chunk | Unreleased (#583) | #576 | `relocate_empty_group_kernel.rs::a_group_this_does_not_relocate_is_refused_before_anything_is_written` |
