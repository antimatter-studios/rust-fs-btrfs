### Added

- **Hard links a log holds are replayed** (#406). A committed file given
  another name after the last commit and made durable by an fsync keeps
  its committed names and gains the new one, with the link count the log
  gives it, and a file created with more than one name is created with
  each of them, as the kernel's own replay leaves them.
