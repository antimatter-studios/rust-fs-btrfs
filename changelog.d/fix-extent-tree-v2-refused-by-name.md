### Fixed

- **An extent-tree-v2 volume is refused by name (#447).** Mounting one was
  refused as `incompatible feature bits 0x0000000000002000 not
  implemented`, which named a mask rather than the feature. The refusal now
  says the volume uses extent-tree-v2, the second-generation extent tree,
  which this driver cannot read. A bit no feature is known by is still
  refused by its value.
- **A read-only mount refuses an unreadable incompat feature too (#447).**
  A primary superblock declaring one was passed over as damaged, so a
  read-only mount fell back to a superblock mirror that did not declare it
  and read the volume as though the feature were not there; a read-write
  mount was refused, but as a damaged primary. Every mount now refuses
  such a volume and names the feature.
