### Fixed

- **A copy-on-write write past the end of a file ending partway through a
  hole allocates that last sector, as the kernel does (#526).** `nbytes` and
  the extent items now match what the kernel leaves for the same write, on a
  volume with explicit hole items and on one without.
- **A copy-on-write write past an item `fallocate --keep-size` preallocated
  past the end is written on a volume without no-holes (#526).** It was
  refused. The item stays preallocated and the gap after it becomes a hole
  item, as the kernel leaves them.
