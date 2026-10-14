### Added

- **Truncate, unlink and a replacing rename drop one reference of an
  extent a reflink shares (#262).** They refused a file whose data extent
  had more than one reference. When the extent is newer than the last
  snapshot, its other references are reflinks, and the file's reference is
  now dropped from the extent's record: its inline back reference counted
  down or removed, and the record's total down by one. The extent, its
  checksums and its space stay for the files still reading it. An extent a
  snapshot may share is still refused.
