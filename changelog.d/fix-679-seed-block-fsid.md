### Fixed

- **A sprouted pool reads the tree blocks its seed devices wrote (#679).**
  Blocks written before a filesystem sprouted are on its seed and carry
  the seed's fsid. They were refused as another filesystem's
  (`fsid … is not this volume's …`), so walking a sprouted pool's trees
  and creating a subvolume on it failed. `mount_pool` and `mount_pool_rw`
  now record each seed's fsid, and every read of the pool accepts it, as
  the kernel's `check_tree_block_fsid` does. `TreeBlock::parse_accepting`
  and `Tree::with_seed_fsids` carry the seeds' fsids for callers that walk
  trees themselves.
