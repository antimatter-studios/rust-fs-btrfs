# Zoned volumes

The read path accepts the ZONED incompatibility bit only with conventional,
fixed-offset superblocks, as used when zones are emulated on a regular device.
Zone allocation changes where extents are placed, not how their chunk mappings
or contents are read. Writes remain refused because the allocator does not
implement sequential zone allocation and reset.

The clean-room format reference is the Btrfs project's
[zoned mode documentation](https://btrfs.readthedocs.io/en/latest/Zoned-mode.html),
especially its superblock section. Real zoned devices use a superblock ring
starting at offset zero. A checksummed ZONED superblock there causes a named
refusal before any conventional copy is selected. This is a conservative
signature check, not support for locating the latest log record.

The `features` fixture recipe formats a sparse regular file with the guest's
static `mkfs.btrfs -O zoned`, mounts it with the guest kernel, records each
file's size and SHA-256, and requires `btrfs check --readonly` to succeed.
`zoned_oracle.rs` compares the driver's reads with that manifest and checks the
volume with the driver's checker. No implementation source was used.
