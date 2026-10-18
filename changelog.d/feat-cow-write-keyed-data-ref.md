### Added

- A copy-on-write write into a shared extent whose reference the kernel filed as a keyed `EXTENT_DATA_REF` item (an extent reflinked into many files) drops that item, counting it down or deleting it, and lowers the record's total, instead of refusing the write.
