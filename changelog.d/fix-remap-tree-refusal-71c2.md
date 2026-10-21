### Fixed

- A remap-tree volume is refused by name at both read-only and read-write
  mount, explaining that the driver cannot read its block-address remap
  tree; unknown incompatible bits are still reported by value (#270).
