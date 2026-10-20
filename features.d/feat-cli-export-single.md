## Interfaces

| `fs.btrfs export`: bounded single-file export with SHA-256, complete-file publication, explicit replacement and raw stdout, including subvolume and snapshot paths (`--features cli`) | Supported | Unreleased (#542) | #540 | `cli_export.rs`, `cli_export_kernel.rs` |
| `fs.btrfs export --recursive`: shared tree and metadata export | Not supported | — | #541 | Blocked until rust-fs-core tree source traits are released |
