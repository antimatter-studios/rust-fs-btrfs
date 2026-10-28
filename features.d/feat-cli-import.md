## Interfaces

| `fs.btrfs import`: a local file, stdin or with `--recursive` a local tree, staged, written in bounded chunks and published by one committed rename only when complete; `--replace` replaces in that transaction, `--verify` reads the staged file back first (`--features cli`) | Supported: the top-level subvolume; links, times and holes (`--preserve`) are refused | Unreleased (#431) | | `cli_import.rs`, `cli_import_kernel.rs` |
