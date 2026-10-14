### Added

- **Copies are written compressed as LZO and zstd too, and a mount can
  ask for compression (#265).** A file whose compression property names
  LZO or zstd was stored uncompressed; it is now written in that
  algorithm, in the framing the kernel reads: LZO as one segment per
  sector behind a length that never straddles a sector boundary, zstd as
  one frame whose window is a single compression unit. The property alone
  is enough, and `none` declines, as the kernel reads it.
  `Filesystem::set_compression` and `fs_btrfs_set_compression` are the
  kernel's `compress=` mount option. A copy is kept compressed only when
  that saves a sector, a `nodatasum` file is never compressed, and the
  first LZO or zstd extent on a volume sets the feature flag announcing
  it, in the same commit. A copy longer than one compression unit
  (128 KiB) is still stored uncompressed (#357).
