### Fixed

- A copy-on-write write within the file's size that starts before a
  preallocated item, and covers the whole of it, now succeeds when the file's
  other items still name that item's extent. On a no-holes volume the range is
  copied into one new extent and the item drops its reference; on a volume
  with explicit hole items the preallocated sectors are written where their
  extent lies and joined to a written neighbour. Both are what the kernel
  does. Such a write was refused, on explicit-hole volumes after its first
  part had been committed (#779).
