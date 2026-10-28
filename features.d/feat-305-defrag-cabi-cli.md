## Writing

| Defragment through the C ABI and the command line (`fs_btrfs_defragment`, `fs.btrfs defragment [--target SIZE] PATH`): the library's defragment of one file, with its counts of extent items before and after, runs and bytes | Supported as the library's defragment is; a missing path or one that is not a regular file, a target shorter than a sector and an extent a snapshot may share are refused | Unreleased (#305) | | `capi_defrag.rs`, `cli_defrag.rs`, `cli_defrag_kernel.rs` |
