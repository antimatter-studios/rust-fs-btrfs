### Added

- **`fs.btrfs subvolume` creates a subvolume and sets its flags (#267).**
  `subvolume create [--mode OCTAL] PATH` makes an empty subvolume in a
  directory of the top-level subvolume and reports its id,
  `subvolume set-read-only ID true|false` makes one read-only or writable
  again, and `subvolume set-default ID` names the one a mount without
  `subvol=` shows. A name already there, a missing parent and an id no
  subvolume has are refused with exit 4, and what the library refuses
  with exit 3.
