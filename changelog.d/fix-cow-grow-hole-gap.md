### Fixed
- A copy-on-write write past EOF on a volume without no-holes extends an existing trailing hole item across its new gap instead of recording two adjacent hole items.
