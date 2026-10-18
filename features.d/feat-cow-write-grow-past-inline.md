## Writing

| A copy-on-write write that grows a file past what its inline data holds, or starts past its end, moving the inline data into a sector-aligned regular extent at offset 0 | Supported | Unreleased (#507) | #262 | `cow_write_oracle.rs` |
