## Writing

| Removing a tree leaf a transaction empties | Experimental; empty nodes leave recursively and an empty root node becomes one empty leaf; shared blocks remain refused | Unreleased (#549, #611, #613) | #444 | `emptied_leaf_kernel.rs`, `cow_write_oracle::a_write_emptying_the_checksum_trees_first_leaf_removes_it_and_the_checker_agrees`, `cow_write_oracle::truncating_the_only_checksummed_file_collapses_its_checksum_root` |
