## Writing

| Balance (`Filesystem::balance`, `fs_btrfs_balance`, `fs.btrfs balance -d/-m`): the block groups the `kinds`, `usage`, `profiles` and `convert` filters select, soft, each relocated into a new chunk | Experimental: single and DUP groups, every group selected checked before the first moves; system groups, other conversions and the `devid`, `drange`, `vrange`, `limit` and `stripes` filters are refused, and `-m` leaves system groups where they are | Unreleased (#729) | #704, #705, #706 | `capi_balance.rs`, `capi_balance_kernel.rs`, `cli_balance.rs`, `cli_balance_kernel.rs` |
