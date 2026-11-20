### Added

- **rust-fs-core's three API layers.** `layers::BtrfsVolume` implements `VolumeAPI` and `OpenVolume`, `PrimitiveAPI`, and `FileAPI`, and `layers::BtrfsAdapter` implements `SalvageAdapter`.
  - **Volume:** probe, open (validating) and open for salvage (from any superblock copy that decodes); geometry; every superblock copy with its integrity; what the driver supports on the volume; and a health verdict from the superblock copies, the chunk map and every tree block met.
  - **Primitive:** the superblock copies and the chunk, root and filesystem tree roots, each copy with its integrity.
  - **Salvage verbs:**
    - Find candidates: inode items in filesystem tree leaves found by scanning; live when the current trees reach the leaf, of unknown state when only an older generation or an orphan holds it.
    - Walk extents: through the chunk map to device bytes; compressed runs are encoded, inline data is an extent inside its leaf.
    - Rebuild the tree: parent claims from `INODE_REF` and `ROOT_BACKREF` items.
    - Report damage, read by extent and decode an encoded extent.
    - Missing features: deleted candidates and extents (Btrfs records no deletion), cluster steps and geometry hypotheses.
  - **Files:** browse, read and copy, crossing into subvolumes as the kernel does: a snapshot's copy of a nested subvolume is an empty directory.
- `layers::strict_read_all` opens a source through the validating mount and reads everything it presents.

### Changed

- rust-fs-core is pinned at `=0.5.1` from crates.io, with its `layers` feature; serde is a new dependency, for the salvage candidate detail. The family scripts move to v0.5.1 with it, and AGENTS.md takes the agent-core v10 block they check.
