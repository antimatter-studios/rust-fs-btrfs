### Fixed

- **A rename that replaces a file's last name gives its data back
  (#262).** Renaming onto the last name of a file that still held data
  extents was refused. The replaced file's extents, their back references,
  their checksums and its block group's count now go in the same
  transaction as the names, as unlink's do. Data a snapshot or a reflink
  shares is still refused, as it is for unlink and truncate.
