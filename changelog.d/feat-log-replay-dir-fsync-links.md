### Added

- **A hard link made durable by an fsync of its directory is replayed**
  (#461). A committed file given another name after the last commit,
  with only its directory fsynced, is logged only to say it exists, its
  committed name outside the range of the directory the log speaks for;
  it keeps that name and gains the new one, with the link count its
  names give it, as the kernel's own replay leaves it.
