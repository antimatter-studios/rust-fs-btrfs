### Added

- Truncate shrinks and grows files inside a selected subvolume while preserving snapshot-shared data and keeping the handle on that subvolume across commits.

### Fixed

- In-place write eligibility uses the selected subvolume's last snapshot generation instead of the top-level tree's generation.
