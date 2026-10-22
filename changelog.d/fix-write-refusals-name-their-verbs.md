### Changed

- **`fs.btrfs write` names the verb a refused change belongs to.** A path
  that does not exist is refused naming `create`, and an input shorter than
  the file naming `truncate`, instead of calling either change something
  the library does not do yet; both still exit 3 and leave the image as it
  was. The README, the C header and `fs.btrfs`'s help describe the name and
  length changes the library makes, and cite the open issues for what they
  still refuse (#421, #426, #428).
