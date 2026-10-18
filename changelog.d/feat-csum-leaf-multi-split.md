### Added

- A transaction splits an overflowing checksum leaf into as many blocks as its items need, carrying every new pointer through full ancestors and recording every allocated block.

### Changed

- `transaction::Split::new` holds all additional block addresses, and `leaf_edit::split_to_fit` returns every leaf partition.
