//! Changing what an inode says about itself: extended attributes, mode,
//! owner and times (#263).
//!
//! Each change is one committed transaction through the same planner as
//! [`crate::namespace`], and touches only the fs tree:
//!
//! - an **extended attribute** is a record in the `XATTR_ITEM` filed
//!   under `(ino, XATTR_ITEM, name_hash(name))`, laid out as a directory
//!   entry with the value after the name (see [`crate::xattr`]). Setting
//!   one replaces the record of that name, or adds it — beside any other
//!   names sharing the hash; removing one deletes the record, and the
//!   item with it when nothing else shares it. ACLs are attributes too
//!   (`system.posix_acl_access`, `system.posix_acl_default`), and are set
//!   and removed the same way;
//! - **mode**, **owner** and **times** are fields of the `INODE_ITEM`,
//!   rewritten in place.
//!
//! Every change also moves the inode's change time, its transaction id
//! and its change counter, as the kernel does.
//!
//! # What it refuses
//!
//! Anything outside the top-level subvolume; an attribute whose records
//! under one hash would outgrow a leaf; and a leaf with no room for a
//! new attribute item, since splitting an fs tree leaf is not written.

use crate::chunk::DiskKey;
use crate::dir::{self, ftype, XATTR_ITEM_KEY};
use crate::error::{Error, Result};
use crate::fs::Filesystem;
use crate::inode::{offsets as io, INODE_ITEM_KEY};
use crate::leaf_edit::OwnedItem;
use crate::transaction::ItemEdit;

/// The longest attribute name Linux accepts, `XATTR_NAME_MAX`.
pub const MAX_XATTR_NAME: usize = 255;

/// The file-type bits of a mode, which a mode change keeps.
const S_IFMT: u32 = 0o170_000;

/// A time to set: seconds since the epoch (negative before it) and
/// nanoseconds.
pub type Time = (i64, u32);

impl Filesystem {
    /// Set the extended attribute `name` (fully qualified: `user.colour`)
    /// on inode `ino` to `value`, replacing any value it had.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless mounted with [`Filesystem::mount_rw`],
    /// [`Error::NotFound`] for no such inode, and
    /// [`Error::UnsupportedFeature`] for a name that is empty, longer than
    /// [`MAX_XATTR_NAME`] or holds a NUL, for a value too large to store,
    /// and for every refusal the [module documentation](crate::attrs)
    /// lists. Nothing is written unless everything is.
    pub fn set_xattr(&mut self, ino: u64, name: &[u8], value: &[u8]) -> Result<()> {
        self.require_writable_top_level()?;
        if name.is_empty() || name.len() > MAX_XATTR_NAME || name.contains(&0) {
            return Err(Error::UnsupportedFeature(format!(
                "{:?} is not an attribute name: 1 to {MAX_XATTR_NAME} bytes, with no NUL",
                String::from_utf8_lossy(name)
            )));
        }
        let generation = self.next_generation()?;
        let now = crate::namespace::now();
        let mut raw = self.raw_inode(ino)?;
        let k = key(ino, XATTR_ITEM_KEY, dir::name_hash(name));
        let existing = self.item_bytes(k)?.unwrap_or_default();
        let mut records = crate::namespace::without_dir_record(&existing, name).unwrap_or(existing);
        records.extend(xattr_record(generation, name, value));
        if records.len() > self.max_item_size() {
            return Err(Error::UnsupportedFeature(format!(
                "attribute {:?} with a {}-byte value does not fit in one leaf item",
                String::from_utf8_lossy(name),
                value.len()
            )));
        }
        crate::namespace::touch(&mut raw, generation, now, &[io::CTIME]);
        let edits = vec![
            ItemEdit::Put(OwnedItem {
                key: k,
                data: records,
            }),
            ItemEdit::Put(OwnedItem {
                key: key(ino, INODE_ITEM_KEY, 0),
                data: raw,
            }),
        ];
        self.commit_edits(ino, edits, generation, now)
    }

    /// Remove the extended attribute `name` from inode `ino`.
    ///
    /// # Errors
    ///
    /// As [`Filesystem::set_xattr`], and [`Error::NotFound`] when the
    /// inode has no attribute of that name.
    pub fn remove_xattr(&mut self, ino: u64, name: &[u8]) -> Result<()> {
        self.require_writable_top_level()?;
        let generation = self.next_generation()?;
        let now = crate::namespace::now();
        let mut raw = self.raw_inode(ino)?;
        let k = key(ino, XATTR_ITEM_KEY, dir::name_hash(name));
        let existing = self.item_bytes(k)?.ok_or(Error::NotFound)?;
        let rest = crate::namespace::without_dir_record(&existing, name).ok_or(Error::NotFound)?;
        crate::namespace::touch(&mut raw, generation, now, &[io::CTIME]);
        let edits = vec![
            crate::namespace::put_or_delete(k, rest),
            ItemEdit::Put(OwnedItem {
                key: key(ino, INODE_ITEM_KEY, 0),
                data: raw,
            }),
        ];
        self.commit_edits(ino, edits, generation, now)
    }

    /// Set the permission bits of inode `ino` to the low 12 bits of
    /// `mode`, keeping its type.
    ///
    /// # Errors
    ///
    /// As [`Filesystem::set_xattr`].
    pub fn set_mode(&mut self, ino: u64, mode: u32) -> Result<()> {
        self.change_inode(ino, |raw| {
            let old = le32(raw, io::MODE);
            put32(raw, io::MODE, (old & S_IFMT) | (mode & 0o7777));
        })
    }

    /// Set the owner of inode `ino`; `None` leaves that half as it is.
    ///
    /// # Errors
    ///
    /// As [`Filesystem::set_xattr`].
    pub fn set_owner(&mut self, ino: u64, uid: Option<u32>, gid: Option<u32>) -> Result<()> {
        self.change_inode(ino, |raw| {
            if let Some(uid) = uid {
                put32(raw, io::UID, uid);
            }
            if let Some(gid) = gid {
                put32(raw, io::GID, gid);
            }
        })
    }

    /// Set the access and modification times of inode `ino`; `None`
    /// leaves that one as it is.
    ///
    /// # Errors
    ///
    /// As [`Filesystem::set_xattr`], and [`Error::UnsupportedFeature`] for
    /// nanoseconds of a second or more.
    pub fn set_times(&mut self, ino: u64, atime: Option<Time>, mtime: Option<Time>) -> Result<()> {
        for t in [atime, mtime].into_iter().flatten() {
            if t.1 >= 1_000_000_000 {
                return Err(Error::UnsupportedFeature(format!(
                    "{} nanoseconds is not a fraction of a second",
                    t.1
                )));
            }
        }
        self.change_inode(ino, |raw| {
            for (at, t) in [(io::ATIME, atime), (io::MTIME, mtime)] {
                if let Some((sec, nsec)) = t {
                    raw[at..at + 8].copy_from_slice(&sec.to_le_bytes());
                    raw[at + 8..at + 12].copy_from_slice(&nsec.to_le_bytes());
                }
            }
        })
    }

    /// Rewrite inode `ino`'s item with `change` applied and its change
    /// time, transaction and change counter moved, as one transaction.
    fn change_inode(&mut self, ino: u64, change: impl FnOnce(&mut [u8])) -> Result<()> {
        self.require_writable_top_level()?;
        let generation = self.next_generation()?;
        let now = crate::namespace::now();
        let mut raw = self.raw_inode(ino)?;
        change(&mut raw);
        crate::namespace::touch(&mut raw, generation, now, &[io::CTIME]);
        let edits = vec![ItemEdit::Put(OwnedItem {
            key: key(ino, INODE_ITEM_KEY, 0),
            data: raw,
        })];
        self.commit_edits(ino, edits, generation, now)
    }
}

fn key(objectid: u64, key_type: u8, offset: u64) -> DiskKey {
    DiskKey {
        objectid,
        key_type,
        offset,
    }
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// One attribute record: a directory-entry header with no location, the
/// name, then the value.
fn xattr_record(generation: u64, name: &[u8], value: &[u8]) -> Vec<u8> {
    use dir::offsets as d;
    let mut b = vec![0u8; d::NAME];
    b[d::TRANSID..d::TRANSID + 8].copy_from_slice(&generation.to_le_bytes());
    b[d::DATA_LEN..d::DATA_LEN + 2].copy_from_slice(&(value.len() as u16).to_le_bytes());
    b[d::NAME_LEN..d::NAME_LEN + 2].copy_from_slice(&(name.len() as u16).to_le_bytes());
    b[d::TYPE] = ftype::XATTR;
    b.extend_from_slice(name);
    b.extend_from_slice(value);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_attribute_record_parses_back_as_what_was_written() {
        let mut item = xattr_record(5, b"user.a", b"one");
        item.extend(xattr_record(5, b"user.b", b""));
        let parsed = crate::xattr::parse_xattr_items(&item).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            (parsed[0].name.as_slice(), parsed[0].value.as_slice()),
            (&b"user.a"[..], &b"one"[..])
        );
        assert_eq!(
            (parsed[1].name.as_slice(), parsed[1].value.as_slice()),
            (&b"user.b"[..], &b""[..])
        );
    }
}
