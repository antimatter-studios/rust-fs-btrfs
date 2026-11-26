# Recovery criteria, mapped to their evidence

Issue #812 asks that Btrfs use the shared recovery API (rust-fs-core's
`VolumeAPI`, `OpenVolume`, `PrimitiveAPI`, `SalvageAdapter`, `FileAPI`)
without losing readable evidence. The implementation landed in #803–#806,
which are merged. This page maps each criterion to the test that proves it,
and says where the proof still waits on CI. The layer design is in
`docs/three-layers.md`.

Status key: **Merged** means the code and its test are on `main`. **CI-only**
means the check runs only in the harness VM or the merge queue, so this page
does not claim a local pass.

| Criterion | Evidence on `main` | Status |
|---|---|---|
| CORE-01 pin | `Cargo.toml` pins `rust-fs-core = "=0.5.1"` from the registry, with no path override; `Cargo.lock` resolves 0.5.1 from the registry; `chores.yml` `FS_CORE_REF` is `v0.5.1`. Pin-agreement tests run in the unit tier. | Merged. The downstream git-revision build is CI-only. |
| CORE-02 primitives | `salvage::superblock_copies`, `root_tree_candidates`, `scan_tree_blocks`, `walk_tree`; `tests/salvage_superblock_oracle.rs`, `tests/salvage_roots_oracle.rs`, `tests/salvage_tree_oracle.rs` (a leaf broken in every copy is reported and the walk goes on). | Merged. |
| CORE-03 checksums | `salvage::CsumTree` (`Recorded`, `Missing`, `Unreadable`), `verify_data`, `SectorTally`; `tests/salvage_data_oracle.rs` (`healthy_data_verifies_as_btrfs_check_says`, `a_damaged_data_sector_is_named_as_btrfs_check_names_it`). | Merged. |
| CORE-04 device bytes | `Layout::physical_runs`, `ExtentClaim::inline_logical`; `tests/salvage_data_oracle.rs::extents_lie_where_btrfs_map_logical_says`. | Merged. |
| CORE-05 truncation | `tests/salvage_truncated_oracle.rs`: `cut_at_64_mib_every_file_is_still_recovered`, `cut_at_64_mib_a_broken_first_copy_has_no_second_to_fall_back_on`, `cut_through_the_filesystem_root_the_loss_is_named`. The last names the loss and claims no recovered files. | Merged. |
| CORE-06 volume and primitive | `layers::BtrfsVolume` implements `VolumeAPI`, `OpenVolume`, `PrimitiveAPI`; `probe`, `open`, `open_for_salvage`; `geometry` reports `layout: None`. `tests/layers_conformance.rs` (`healthy_fixtures_conform`, `damaged_copies_conform`). | Merged. |
| CORE-07 salvage adapter | `layers::BtrfsAdapter` implements `SalvageAdapter`; `tests/layers_conformance.rs` and `tests/layers_salvage_conformance.rs` (`the_salvage_adapter_conforms`). Named gaps: `DeletedCandidates`, `DeletedExtents`, `ClusterSteps`, `GeometryHypotheses`. | Merged. The named gaps are open work, not claimed here. |
| CORE-08 kernel match | `tests/layers_kernel_oracle.rs`: `the_file_layer_lists_what_the_kernel_lists`, `the_salvage_verbs_rebuild_what_the_kernel_lists`, on six fixtures including subvolumes and snapshots. | Merged. |
| CORE-09 oracle targets | Floors in `.github/test-floors.txt` (`layers_conformance` 2, `layers_salvage_conformance` 1, `layers_kernel_oracle` 2, plus the salvage oracles). No expected-failure or ignored entries remain in `tests/` or `src/`. | Merged. Full-depth pass is CI-only. |
| CORE-10 openers | `Filesystem::mount`, `mount_with_cache`, `mount_ignoring_log` deprecated from 0.12.0 as thin wrappers (#806). `open_for_salvage` browses committed trees past an unreplayed log and still reports the log (`src/layers.rs`, `src/log_tree.rs`). | Deprecation merged. The dirty-log browse has no test located by name in `tests/layers_*.rs`, so its evidence is not yet mapped. |

## Still open

- Downstream git-revision build (CORE-01), full-depth oracle run (CORE-09),
  and the baseline tiers rerun (issue completion gate): CI-only.
- The four named salvage gaps listed under CORE-07.
- CORE-10's dirty-log browse: map it to a named test.
- Issue #812 stays open until a maintainer checks these rows against a green
  run.
