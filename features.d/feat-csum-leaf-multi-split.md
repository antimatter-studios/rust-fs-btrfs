## Writing

| Multi-way checksum leaf splits | Experimental; overflowing leaves and ancestors split into multiple blocks in one transaction | Unreleased (#619) | #615, #550 | `cow_write_oracle::a_write_adding_more_than_two_leaves_of_digests_splits_the_checksum_leaf_as_often_as_needed`, `transaction::split_tests` |
| Multiple new root levels in one transaction | Refused by name; one new root cannot yet split again | Unreleased (#619) | #617, #550 | |
