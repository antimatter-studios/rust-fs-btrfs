## Writing

| A growing copy-on-write write from a file ending partway through a hole, allocating that last sector | Supported | Unreleased (#766) | #526 | `cow_write_oracle.rs::a_growing_write_allocates_an_old_eof_sector_in_a_hole_as_the_kernel_does` |
| A growing copy-on-write write wholly past an item preallocated past the end with `fallocate --keep-size`, leaving the item preallocated | Supported | Unreleased (#766) | #526 | `cow_write_oracle.rs::a_growing_write_past_space_preallocated_past_the_end_leaves_it_preallocated` |
