### Added

- **Defragment (#305).** `Filesystem::defragment` and `defragment_to`
  rewrite each run of a file's adjacent extents shorter than a target size
  (32 MiB by default, as `btrfs filesystem defragment -t` uses) as one new
  extent, copy-on-write and in one committed transaction: the run's first
  item covers the copy, the items after it are deleted, the old extents
  are released with their checksums, and the copy gains a digest per
  sector. An extent a reflink shares is copied too and the file's
  reference to it dropped, which breaks the reflink as the kernel does by
  default. Inline, preallocated and compressed extents, holes, and an
  extent the file references more than once are left as they are; an
  extent a snapshot may share is refused.
