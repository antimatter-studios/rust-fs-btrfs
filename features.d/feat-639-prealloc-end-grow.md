## Writing

| A growing copy-on-write write that starts in an item preallocated past the end with `fallocate --keep-size` and runs past that item's end, written in two at that end: the part in the item where its extent lies, the rest as any growing write | Supported | Unreleased (#639) | #538 | `cow_write_oracle.rs`, `cow_write.rs` |
