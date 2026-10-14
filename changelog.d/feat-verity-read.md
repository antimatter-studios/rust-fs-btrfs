### Added

- **fs-verity files are read and checked (#270).** A file sealed with
  `fsverity enable` has every block a read touches hashed and checked
  against its Merkle tree up to the root hash in its descriptor, as the
  kernel checks it, and a block that does not verify is refused with
  `Error::ChecksumMismatch` (`EIO` through the C ABI) rather than returned.
  `Filesystem::verity` returns a sealed file's descriptor, with its digest
  as `fsverity measure` prints it, and `Filesystem::verity_merkle_tree` its
  stored Merkle tree; the new `verity` module parses both, for SHA-256 and
  SHA-512, salted or not. `tests/verity_oracle.rs` holds each to what the
  kernel reports for a volume it sealed, and has a tampered block refused
  where the kernel refuses it. A volume with verity files is still refused
  read-write.
