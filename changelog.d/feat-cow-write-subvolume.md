### Added

- **A copy-on-write write reaches files inside a subvolume other than the
  top level (#445).** A handle from `open_subvolume` on a writable mount
  refused `write` as read-only. Its copy-on-write writes now plan against
  the subvolume's own tree, check its `ROOT_ITEM`'s `last_snapshot`, and
  name that tree in the data references and in the commit, and the handle
  stays on the subvolume afterwards. `tests/subvol_cow_write_kernel.rs`
  writes into a kernel-made file inside a subvolume and has
  `btrfs check --check-data-csum` and a scrub pass, the kernel read the
  bytes back through a `subvol=` mount, and the top level unchanged. An
  in-place write to a `nodatacow` file inside a subvolume is still
  refused.
