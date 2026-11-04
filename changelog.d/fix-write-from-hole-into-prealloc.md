### Fixed

- A copy-on-write write within the file's size that starts in an unrecorded
  hole and reaches a preallocated item now leaves the same extent items as
  the kernel. The copied range stays together, the unwritten suffix keeps
  its preallocated extent and reference, and already allocated bytes are
  not counted twice in the inode's `nbytes` (#774).
