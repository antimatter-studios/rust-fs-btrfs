## Writing

rename: A write that grows a file or lands in a hole
| A write into a hole, or one that grows a file from at or before its end, on a no-holes volume | Supported | Unreleased (#432) | #261 | `cow_write_oracle.rs` |
| A write that starts past a file's end, or grows a file on a volume without no-holes | Refused | Unreleased (#432) | #433, #436 | `cow_write_oracle.rs` |
| A write into a hole on a volume without no-holes | Supported | Unreleased (#434) | #434 | `cow_write_oracle.rs` |
