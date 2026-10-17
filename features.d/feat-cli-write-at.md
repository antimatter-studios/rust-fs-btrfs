## Interfaces

| `fs.btrfs write --at BYTES PATH`: positioned writes | Not supported (`not implemented`, exit 3), refused before opening the target, including position zero | Unreleased (#415) | #415 | `cli_dispatch.rs`, `tests/cli/test-blocked.sh`, `cli_docs.rs` |
