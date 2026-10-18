### Added

- **A balance filter selects block groups (#584, part of #400).**
  `balance::Balance` takes the kernel's `kinds`, `usage`, `profiles` and
  `convert` filters, with the kernel's soft behaviour of leaving a group
  already in the target profile where it is, and `Balance::selects` says
  whether a block group is one it relocates; `balance::profile_bits` gives
  a profile's chunk type bits.
