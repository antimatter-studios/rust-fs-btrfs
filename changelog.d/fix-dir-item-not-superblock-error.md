### Fixed

- **A malformed directory item no longer blames the superblock.** Errors
  from damaged metadata read "malformed Btrfs metadata: …" and name the
  structure at fault; a refusal of the superblock itself still says
  "superblock" in its detail.
