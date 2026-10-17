### Fixed

- **A volume with quotas enabled refuses a read-write mount (#270).** Quota
  groups count each subvolume's referenced and exclusive bytes, and the kernel
  updates them in the transaction that moves an extent. This driver does not,
  so a write would have left them wrong. It still mounts read-only, as a
  volume with simple quotas already did.
