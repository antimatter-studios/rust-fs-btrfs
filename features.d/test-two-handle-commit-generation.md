## Writing

| Two open handles of one volume (top level and subvolume) never commit over each other | Supported: the stale one refuses every write with ESTALE before it reads through its root tree | Unreleased (#599, #740) | #599, #715 | `capi_two_handle_commit_kernel.rs`, `stale_top_level_write_contract.rs` |
