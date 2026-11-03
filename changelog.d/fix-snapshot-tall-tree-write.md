### Fixed

- **A write into a snapshot whose tree is taller than one leaf is taken
  (#734).** A handle from `open_subvolume` on such a snapshot refused every
  write that reached a block below the root. The plan now walks the path of
  the tree the handle writes, and copies a block another tree owns, or one
  flagged `FULL_BACKREF`, as the kernel's `update_ref_for_cow` does: its
  children gain references keyed by the copying tree, and the source keeps
  its blocks and its bytes.
- **Copying such a block is taken in shapes that were refused (#734).** A
  block flagged `FULL_BACKREF` that a snapshot still shares is copied, a leaf
  flagged `FULL_BACKREF` splits, a block a snapshot shares leaves the tree a
  change empties it from, and a moved block's references file as items of
  their own when the kernel would, including a leaf naming one extent at
  several inodes or offsets.
