### Added

- **A data block group of more than 64 extents relocated a batch per
  transaction (#494, part of #398).** `Filesystem::relocate_block_group`
  no longer refuses a data group holding more extents than one
  transaction moves: it moves them 64 at a time, committing each batch
  before copying the next, so a crash between batches leaves both groups
  consistent with every extent in exactly one of them. A refusal still
  comes before anything is written and leaves the device byte-identical.
