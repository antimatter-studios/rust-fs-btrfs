## Reading

| A zoned volume on a regular device | Supported, read-only, with fixed-offset superblocks | Unreleased (#565) | #454, #270 | `zoned_oracle.rs` |
| A zoned device with a superblock log | Refused by name before conventional mirror fallback | Unreleased (#565) | #454, #270 | `superblock::tests::a_zoned_superblock_log_is_refused_before_a_stale_fixed_copy` |

## Writing

| A zoned volume | Refused by name before writing | Unreleased (#565) | #454, #270 | `zoned_oracle.rs` |
