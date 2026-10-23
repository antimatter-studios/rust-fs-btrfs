## Writing

| A growing copy-on-write write into space preallocated past the end with `fallocate --keep-size`, written in place when one preallocated item holds the old end, the gap and the write; an unaligned old end or a write leaving the item is refused | Supported | Unreleased (#603) | #526 | `cow_write_oracle.rs` |
