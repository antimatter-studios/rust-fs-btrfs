### Changed

- **`fs.btrfs write --at BYTES` answers that positioned writes are not
  implemented (#415).** It used to be an unknown option, exit 2. It is now
  accepted and refused with exit status 3 and a `not implemented` error,
  before stdin is read or the image is opened, as the other tools answer an
  operation their driver cannot do. Plain `write` is unchanged.
