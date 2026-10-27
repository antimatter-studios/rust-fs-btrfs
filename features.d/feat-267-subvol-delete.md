## Writing

| Subvolumes and snapshots: deleting one (`delete_subvolume`) | Supported from a directory of the top-level subvolume, lazily as the kernel deletes: the entry, both references and the UUID go, and an orphan item leaves the tree to the kernel's cleaner; the default subvolume and one holding another subvolume are refused | Unreleased (#267) | | `subvol_delete_contract.rs`, `subvol_delete_kernel.rs` |
rename: Subvolumes and snapshots: snapshot, delete
| Subvolumes and snapshots: snapshot | Supported (`create_snapshot`) for a source whose tree is one leaf, writable or read-only, into a directory of the top-level subvolume; a taller source tree is refused | Unreleased (#267) | | `subvol_snapshot_contract.rs`, `subvol_snapshot_kernel.rs` |
