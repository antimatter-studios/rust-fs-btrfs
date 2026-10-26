### Fixed

- **A stale handle no longer commits over another handle's write.** When the top level and a subvolume of one volume are both open and one commits, a write through the other is refused with `ESTALE` instead of putting the older tree back and reusing the generation; open the handle again to write.
