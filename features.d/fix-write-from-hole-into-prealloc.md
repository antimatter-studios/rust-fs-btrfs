## Writing

| A copy-on-write write within the file's size from an unrecorded hole into a preallocated item leaves the kernel's extent layout, kinds, reference counts and inode accounting on both hole representations | Supported | Unreleased (#777) | #774 | `cow_write_oracle.rs::a_write_within_size_from_a_hole_into_preallocation_matches_the_no_holes_kernel`, `cow_write_oracle.rs::a_write_within_size_from_a_hole_into_preallocation_matches_the_explicit_holes_kernel` |
