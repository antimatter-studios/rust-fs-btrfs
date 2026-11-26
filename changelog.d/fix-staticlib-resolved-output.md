### Fixed

- `chore staticlib` copies the archive from the directory cargo built it in.
  With `CARGO_TARGET_DIR` or `build.target-dir` set, it failed, or shipped an
  older archive still in `target/` without saying so (#819).
