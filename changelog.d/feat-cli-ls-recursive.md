### Added

- `fs.btrfs ls --recursive` (`-R`) walks in byte-sorted depth-first order, entering and marking subvolumes and snapshots without following symlinks. Entries add absolute paths and relative depths; repeated real directories are marked and not entered again. Depth and entry bounds default to 64 and 100000, exact fits succeed, and a bound or unreadable directory retains the collected report on stdout with exit 1. The kernel-oracle test compares nested subvolumes and an ancestor snapshot with GNU find in the test VM.
