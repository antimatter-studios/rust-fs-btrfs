### Added

- **Salvaged file data is checked against the checksum tree, and every extent is placed on the device.**
  - `salvage::CsumTree` looks up each data sector's digest past damaged blocks.
  - `salvage::verify_data` checks bytes sector by sector. It replaces a sector that fails with another copy that matches, and names the sectors that match on no copy.
  - `salvage::read_file_extent` and `salvage::copy_file` take the checksum tree, report `DataFault::Checksum` for damaged sectors (kept as read, as evidence), and tally verified, unchecked and failed sectors.
  - `Layout::physical_runs` gives the device ranges a logical range occupies, and `ExtentClaim::inline_logical` gives where an inline extent's bytes are inside its leaf.
