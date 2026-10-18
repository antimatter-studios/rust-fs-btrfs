### Added

- **A logged write inside a compressed extent is replayed** (#409). A
  write into part of a committed compressed extent, made durable by fsync
  alone, leaves the extent's head and tail as windows into it; replay now
  cuts the committed item into those windows, keeping its reference to the
  whole compressed extent, without decoding it, as the kernel's own replay
  does. A log that needed such a cut was refused before.
