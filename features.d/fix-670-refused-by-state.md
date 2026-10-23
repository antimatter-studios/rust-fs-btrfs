## Interfaces

| `fs.btrfs` namespace verbs (`rm`, `rmdir`, `mv`, `ln`, `mkdir`, `truncate`) refused by the volume's state (a directory that is not empty, a name already there, a missing target, a part of the path that is not a directory): exit 4, `refused`, as the CLI contract has it | Supported | Unreleased (#670) | #670 | `tests/cli/test-write.sh`, `cli_mkdir_kernel.rs` |
