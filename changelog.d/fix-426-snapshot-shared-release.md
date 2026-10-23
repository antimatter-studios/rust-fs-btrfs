### Added

- **Truncate, unlink and a replacing rename release data a snapshot shares
  (#426).** A shared tree block is copied with full references added for
  its children before the file's reference is dropped, so the snapshot
  keeps the extent. Shared blocks already carrying full backrefs, and
  references stored outside the extent item, remain refused.
