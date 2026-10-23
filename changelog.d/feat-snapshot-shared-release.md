### Added
- Truncate, unlink and replacing rename preserve snapshot-shared data by copying shared tree blocks, adding full references for their children and dropping the active file's reference; shared blocks already carrying full backrefs and references stored outside the extent item remain refused.
