### Changed

- **`fs.btrfs`'s namespace verbs exit 4 when the volume's state refuses them
  (#670).** `rm`, `rmdir`, `mv`, `ln`, `mkdir` and `truncate` return
  `refused`, exit 4, with `"code": 4` in the JSON error, when they meet a
  directory that is not empty, a name already there, a missing target or a
  part of the path that is not a directory. A non-empty `rmdir` and a name
  already there were exit 3 and the rest exit 1 before. Every other failure
  keeps its status, as the CLI contract has it.
- **Built on rust-fs-core 0.3.17**, for `CliError::refused_by_state`.
