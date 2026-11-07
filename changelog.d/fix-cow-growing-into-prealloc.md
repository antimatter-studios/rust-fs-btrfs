### Fixed

- **A growing copy-on-write write into space preallocated past the end is
  written off a sector and from before the item (#767).** It was refused when
  the file's old end lay partway through a sector, when the write started or
  ended partway through a sector inside the item, and when it started before
  the item and ran into it. Each is now written as the kernel writes it: inside
  the item in whole sectors where its extent lies, and from a hole no item
  records into one new extent that cuts the item short.
