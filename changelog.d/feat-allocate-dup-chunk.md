### Added

- **DUP chunks can be allocated on one device for balance (#585, part of #400).**
  The chunk allocator places two non-overlapping stripes, records a device
  extent for each, and charges both stripes to the device's bytes used.
  Profile conversion is the next balance step (#586).
