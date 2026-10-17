### Added

- **The logs of every subvolume are replayed** (#380). When fsyncs in more
  than one subvolume leave a log tree for each, a read-write mount replays
  every one of them into its subvolume, with the same rules as the top
  level's, in one transaction that also clears the log.
