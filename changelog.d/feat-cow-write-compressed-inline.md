### Added

- **A copy-on-write write inside a compressed inline extent (#438).** It
  was refused. The item is decoded, the write applied, and the result
  stored inline again: compressed when the file asks for compression and
  the encoding is smaller, as the kernel decides, otherwise uncompressed.
