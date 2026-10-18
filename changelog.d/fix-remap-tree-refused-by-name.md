### Fixed

- **A remap-tree volume is refused by name (#448).** Mounting one was
  refused as `incompatible feature bits 0x0000000000020000 not
  implemented`, which named a mask rather than the feature. The refusal now
  says the volume uses remap-tree, the tree that remaps block addresses,
  which this driver cannot read.
