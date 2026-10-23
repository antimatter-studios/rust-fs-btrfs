### Added

- **A metadata block group converts between single and DUP (#588, part of #400).**
  `Filesystem::relocate_block_group` with `Some(ChunkProfile::Dup)` or
  `Some(ChunkProfile::Single)` moves a single metadata group into a new DUP
  chunk, or a DUP one into a new single chunk, copying its tree blocks there
  and removing the old chunk, instead of refusing the conversion. System
  groups, every other conversion, and relocating a DUP metadata group without
  one are still refused before anything is written.
