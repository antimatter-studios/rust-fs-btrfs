## Interfaces

| `fs.btrfs export`: bounded single-file export with SHA-256, complete-file publication, explicit replacement and raw stdout, including subvolume and snapshot paths (`--features cli`) | Supported | Unreleased (#542) | #540 | `cli_export.rs`, `cli_export_kernel.rs` |
| `fs.btrfs export --recursive`: shared tree and metadata export | Supported: a directory tree through rust-fs-core's tree transfer, subvolumes and snapshots entered, each item reported; `--preserve=links,times` keeps hard links, symlink targets (never followed) and the stored access and modification times | Unreleased (#541) | | `cli_export_tree.rs`, `cli_export_tree_kernel.rs` |
