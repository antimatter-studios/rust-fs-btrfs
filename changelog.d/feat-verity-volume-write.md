### Added
- Volumes with fs-verity files mount read-write, allowing new files and changes to unsealed files while preserving sealed files and their measurements. Sealed data writes, truncate, clone destinations, unlink and rename replacement are refused with an fs-verity diagnostic.
