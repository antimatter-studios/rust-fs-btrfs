## Writing

rename: A write that grows a file or lands in a hole
| A write into a hole, or one that grows a file from at or before its end, on a no-holes volume | Supported | Unreleased (#432) | #261 | `cow_write_oracle.rs` |
| A write that starts past a file's end, on a no-holes volume | Supported | Unreleased (#433) | #261 | `cow_write_oracle.rs` |
| A write that grows a file on a volume without no-holes, recording the gap as a hole item | Supported | Unreleased (#436) | #261 | `cow_write_oracle.rs` |
| A write into a hole on a volume without no-holes | Supported | Unreleased (#456) | #434 | `cow_write_oracle.rs` |
