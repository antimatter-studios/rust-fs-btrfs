### Fixed

- **An extent-tree-v2 volume is refused by name (#447).** Mounting one was
  refused as `incompatible feature bits 0x0000000000002000 not
  implemented`, which named a mask rather than the feature. The refusal now
  says the volume uses extent-tree-v2, the second-generation extent tree,
  which this driver cannot read. A bit no feature is known by is still
  refused by its value.
