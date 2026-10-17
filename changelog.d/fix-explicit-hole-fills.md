### Fixed
- Copy-on-write writes into sparse files on volumes without the no-holes feature preserve explicit hole items around the new data, preventing overlapping extents and uncovered ranges.
