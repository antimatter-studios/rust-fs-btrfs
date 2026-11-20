### Added

- **Every superblock copy is read with its own verdict, for salvage.** `salvage::superblock_copies` reads all three copies from a read-only source. Each copy is reported as valid, absent, unreadable, not Btrfs, failing its checksum, misplaced, declaring an unreadable feature, or malformed. A copy whose checksum fails still has its fields decoded (`Superblock::parse_unverified`), and `salvage::best_copy` names the copy a mount would use.
- **The backup root ring is read.** `BackupRoots::parse` and `super_write::read_backups` decode the four `btrfs_root_backup` slots in each superblock copy. Those slots hold the older tree roots that a walk of an earlier generation starts from.
