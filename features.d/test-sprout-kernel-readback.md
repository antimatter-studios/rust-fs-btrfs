## Writing

| A filesystem sprouted from a seed, written through `mount_pool_rw` | Experimental; new top-level files are written on the sprout and the seed stays unchanged, checked by btrfs-progs and exact guest-kernel readback; subvolume handles remain unsupported | Unreleased (#627, #629) | #628 | `seed_oracle.rs`, `seed_write_kernel.rs` |
