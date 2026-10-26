## Writing

| Copy-on-write into an extent a snapshot shares, read-only or writable: the copy drops the top level's reference, and the snapshot keeps reading the old extent | Supported on the top-level subvolume | Unreleased (#443) | #261 | `cow_write_oracle.rs` |
