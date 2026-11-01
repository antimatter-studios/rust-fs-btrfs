### Added

- `fs.btrfs TARGET import [--recursive] [--replace] [--verify] SOURCE PATH` streams a local file, stdin or a local tree into the image through rust-fs-core's shared import: each file is staged beside PATH, written copy-on-write in bounded chunks, optionally read back through a fresh open, and published by one committed rename only when complete. A refusal, a full volume or a signal unlinks the staged file and leaves PATH as it was; `--replace` replaces an existing file in the rename's own transaction, never deleting it first.
