### Fixed

- New data extents and tree blocks on a sprouted filesystem use only block
  groups whose stripes are all writable. Seed block groups remain readable
  but are excluded from allocation, including SYSTEM blocks and explicit
  relocation targets. New chunks take their device extents and device UUID
  only from writable members (#625).
