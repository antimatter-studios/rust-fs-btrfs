### Fixed

- A write through a stale top-level handle is refused with `Error::Stale` (`ESTALE` through the C ABI) before it reads through the handle's root tree. It used to fail with `BadSuperblock`, `NotFound` or `ENOENT` once that tree's blocks had been reused.
- `trim`, `scrub_repair` and `write_at` refuse a stale handle. They used to act on the view the handle read at mount.
