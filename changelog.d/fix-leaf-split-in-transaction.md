### Fixed

- **A full leaf is split in the transaction that fills it (#342).** A
  directory, attribute or checksum edit that no longer fits its tree leaf
  splits that leaf in two, adding the new block's pointer to the parent node
  -- or, when the leaf was the tree's root, growing the tree a level -- with
  its extent record and block-group count in the same commit. A directory
  used to refuse its ninth long name on a 4 KiB node. A leaf a snapshot
  shares, a parent node with no room for another pointer, and growing the
  root or chunk tree are still refused. `tests/leaf_split_oracle.rs` has
  `btrfs check` find two hundred names clean and the kernel list them all. **Breaking:** `transaction::Plan` gains
  `splits`, the leaves a plan splits (`transaction::Split`), so 0.11.0.
