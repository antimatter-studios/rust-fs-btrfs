### Fixed

- A growing copy-on-write write crossing a hole into kept preallocation from
  a dirty regular sector now preserves the untouched extent window and leaves
  the kernel's items on volumes with and without no-holes (#772). With an old
  end partway through a sector, the dirty regular window joins the adjacent
  hole's copy; with an aligned old end it stays separate.
- A growing write past an old end partway through a preallocated sector left
  by shrinking truncate now zeroes that sector in place and updates its item,
  references and checksums in the same transaction as the write (#773).
