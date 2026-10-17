## Reading

| A volume with quota groups enabled (`btrfs quota enable`) | Supported, read-only; refused read-write, since a write would leave the quota groups' referenced and exclusive counts wrong | Unreleased (#270) | #270 | `qgroup_kernel.rs` |
