## Writing

| A growing copy-on-write write into space preallocated past the end with `fallocate --keep-size`, written in place when one preallocated item holds the old end, the gap and the write; an unaligned old end, a write ending off a sector inside the item, and a write from before the item into it are refused | Supported | Unreleased (#603) | #526, #767 | `cow_write_oracle.rs::a_growing_write_into_space_preallocated_past_the_end_is_written_in_place`, `cow_write_oracle.rs::a_growing_write_into_kept_preallocation_refuses_what_it_cannot_write` |
