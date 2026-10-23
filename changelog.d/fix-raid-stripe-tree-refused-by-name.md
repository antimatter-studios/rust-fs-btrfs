### Fixed

- **A raid-stripe-tree volume is refused by name (#449).** Mounting one was
  refused as `incompatible feature bits 0x0000000000004000 not
  implemented`, which named a mask rather than the feature. The refusal now
  says the volume uses raid-stripe-tree, the tree that records data stripe
  placement, which this driver cannot read.
