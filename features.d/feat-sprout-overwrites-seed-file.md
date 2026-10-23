## Writing

| Overwriting a seed-resident file in a sprouted filesystem | Experimental; the new data is written copy-on-write onto the sprout, the old seed extents are released from the sprout's accounting only, and the seed stays unchanged, checked by btrfs-progs and exact guest-kernel readback | Unreleased | #563 | `seed_oracle.rs` |
