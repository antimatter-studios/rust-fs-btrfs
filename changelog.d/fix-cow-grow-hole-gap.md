### Fixed
- A copy-on-write write past EOF on a volume without no-holes records its new gap as a hole item of its own beside an existing trailing hole item, as the kernel does.
