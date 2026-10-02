//! Writing an ordinary, copy-on-write file.
//!
//! [`crate::write`] overwrites a `nodatacow` file where its bytes lie. A
//! copy-on-write file may not be written that way: its extents are
//! immutable once committed, so changing a byte means a new extent.

use crate::error::{Error, Result};
use crate::fs::Filesystem;
use crate::inode::INODE_NODATACOW;

impl Filesystem {
    /// Write `data` at `offset` in a regular file.
    ///
    /// A `nodatacow` file is written in place, exactly as
    /// [`Filesystem::write_at`] does.
    pub fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize> {
        if self.writable.is_none() {
            return Err(Error::ReadOnly);
        }
        if data.is_empty() {
            return Ok(0);
        }
        let inode = self.read_inode(ino)?;
        if !inode.is_regular_file() {
            return Err(Error::NotAFile);
        }
        if inode.flags & INODE_NODATACOW != 0 {
            return self.write_at(ino, offset, data);
        }
        self.write_at(ino, offset, data)
    }
}
