//! Names in a directory: create, mkdir, symlink, link, unlink and rmdir
//! (#262).
//!
//! A name in a Btrfs directory is four items in the fs tree, and a new
//! file is one or two more:
//!
//! - the child's `INODE_ITEM` (a new one, or the existing one with its
//!   link count raised, for a hard link);
//! - an `INODE_REF` under the child, keyed by the parent, holding the
//!   name and its index in the parent — what `..` and a path back up
//!   are read from;
//! - a `DIR_ITEM` under the parent, keyed by the name's hash, for lookup
//!   by name (several names sharing a hash share the item);
//! - a `DIR_INDEX` under the parent, keyed by that index, for listing
//!   in creation order;
//! - the parent's `INODE_ITEM`, whose size counts every name twice (once
//!   for each of the two entries) and whose times move;
//! - for a symbolic link, an inline `EXTENT_DATA` item holding the
//!   target.
//!
//! None of these allocates data: an empty file, a directory and a
//! symbolic link hold no extent, so the extent tree changes only by the
//! tree blocks the transaction rewrites. Each operation is one committed
//! transaction through the same planner and commit as
//! [`crate::cow_write`], so a crash leaves the directory as it was or as
//! it became.
//!
//! # What it refuses
//!
//! - anything outside the top-level subvolume (tree 5), which is the
//!   only tree a mount writes;
//! - a leaf with no room for the new items: splitting an fs tree leaf is
//!   not implemented, so on a large directory a new name can be refused
//!   with nothing written;
//! - a hard link to a directory, and a link whose `INODE_REF` item for
//!   that parent would outgrow a leaf;
//! - a symbolic link whose target does not fit inline in one sector.
//!
//! - removing the last name of a file that still holds data extents:
//!   releasing an extent is not implemented outside a copy-on-write
//!   write, so such a file is refused whole rather than left leaking;
//! - removing a directory that is not empty.
//!
//! Removing a name is the same items in reverse: the entries and the
//! reference lose the name (an item holding nothing else is deleted),
//! the parent's size and times move, and an inode left with no name is
//! deleted together with its attributes and inline data.
//!
//! Renaming and truncating are the rest of #262 and not here yet.

use std::sync::Arc;

use crate::chunk::{objectid, DiskKey};
use crate::dir::{self, ftype, DIR_INDEX_KEY, DIR_ITEM_KEY, MAX_NAME_LEN};
use crate::error::{Error, Result};
use crate::fs::{Filesystem, EXTENT_DATA_KEY};
use crate::inode::{
    offsets as io, Inode, FIRST_FREE_OBJECTID, INODE_ITEM_KEY, INODE_ITEM_SIZE, INODE_NODATACOW,
    INODE_NODATASUM, INODE_REF_KEY,
};
use crate::leaf_edit::OwnedItem;
use crate::super_write::Commit;
use crate::transaction::{DataWrite, ItemEdit};

/// How many rounds a change's plan has to close over its own bookkeeping.
const PLAN_ROUNDS: usize = 64;

/// The highest objectid an inode may have: `BTRFS_LAST_FREE_OBJECTID`.
/// The tree's special items (orphans, the free-inode cache) live above.
const LAST_FREE_OBJECTID: u64 = u64::MAX - 256;

/// The first `DIR_INDEX` a directory hands out; 0 and 1 were once `.`
/// and `..`.
const FIRST_DIR_INDEX: u64 = 2;

/// The file-type bits of a mode, and the types this module makes.
const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;

/// `BTRFS_INODE_COMPRESS` and `BTRFS_INODE_NOCOMPRESS`, which a new
/// inode takes from its directory along with `NODATACOW`.
const INODE_NOCOMPRESS: u64 = 1 << 3;
const INODE_COMPRESS: u64 = 1 << 11;

/// The fixed part of an `INODE_REF`: the index and the name's length.
const INODE_REF_HEADER: usize = 8 + 2;

/// What a new inode is.
#[derive(Debug, Clone, Copy)]
enum NewKind<'a> {
    File,
    Dir,
    Symlink(&'a [u8]),
}

impl Filesystem {
    /// Create an empty regular file `name` in directory `parent`, with
    /// permission bits `mode` (the low 12 bits), owned by `uid`:`gid`.
    /// Returns the new inode number.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless mounted with [`Filesystem::mount_rw`];
    /// [`Error::NotADirectory`] if `parent` is not one;
    /// [`Error::UnsupportedFeature`] for a name that is invalid or
    /// already present, and for every refusal the [module
    /// documentation](crate::namespace) lists. Nothing is written unless
    /// everything is.
    pub fn create(
        &mut self,
        parent: u64,
        name: &[u8],
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<u64> {
        self.add_inode(parent, name, NewKind::File, mode, uid, gid)
    }

    /// Create an empty directory `name` in `parent`. As
    /// [`Filesystem::create`].
    pub fn mkdir(
        &mut self,
        parent: u64,
        name: &[u8],
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<u64> {
        self.add_inode(parent, name, NewKind::Dir, mode, uid, gid)
    }

    /// Create a symbolic link `name` in `parent` pointing at `target`,
    /// stored inline. As [`Filesystem::create`]; a target that is empty
    /// or does not fit inline in one sector is refused.
    pub fn symlink(
        &mut self,
        parent: u64,
        name: &[u8],
        target: &[u8],
        uid: u32,
        gid: u32,
    ) -> Result<u64> {
        self.add_inode(parent, name, NewKind::Symlink(target), 0o777, uid, gid)
    }

    /// Add the name `name` in `parent` for the existing inode `ino`: a
    /// hard link. A directory cannot be linked.
    ///
    /// # Errors
    ///
    /// As [`Filesystem::create`], and [`Error::NotAFile`] for a
    /// directory.
    pub fn link(&mut self, ino: u64, parent: u64, name: &[u8]) -> Result<()> {
        self.require_writable_top_level()?;
        let dir = self.writable_parent(parent, name)?;
        let target = self.read_inode(ino)?;
        if target.is_dir() {
            return Err(Error::NotAFile);
        }
        let generation = self.next_generation()?;
        let now = now();
        let index = self.next_dir_index(parent)?;

        let mut edits = Vec::new();
        // The inode: one more link, changed now.
        let mut raw = self.raw_inode(ino)?;
        let nlink = le32(&raw, io::NLINK);
        if nlink >= u32::from(u16::MAX) {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} already has {nlink} links, the most Btrfs allows"
            )));
        }
        raw[io::NLINK..io::NLINK + 4].copy_from_slice(&(nlink + 1).to_le_bytes());
        touch(&mut raw, generation, now, &[io::CTIME]);
        edits.push(put(ino, INODE_ITEM_KEY, 0, raw));
        // Its reference back to this parent, joining any it has there.
        let ref_key = key(ino, INODE_REF_KEY, parent);
        let mut refs = self.item_bytes(ref_key)?.unwrap_or_default();
        refs.extend(inode_ref(index, name));
        if refs.len() > self.max_item_size() {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino}'s references from directory {parent} would outgrow a leaf, and \
                 extended references are not written"
            )));
        }
        edits.push(ItemEdit::Put(OwnedItem {
            key: ref_key,
            data: refs,
        }));
        self.entry_edits(
            &mut edits,
            &dir,
            name,
            ino,
            file_type(&target),
            index,
            generation,
            now,
        )?;
        self.commit_edits(ino, edits, generation, now)
    }

    /// Remove the name `name` from directory `parent`. When it was the
    /// inode's last name, the inode goes too, with every item it owns.
    ///
    /// # Errors
    ///
    /// As [`Filesystem::create`]; [`Error::NotFound`] when there is no
    /// such name, [`Error::NotAFile`] for a directory (use
    /// [`Filesystem::rmdir`]), and [`Error::UnsupportedFeature`] for the
    /// last name of a file that still holds data extents: releasing them
    /// is not implemented yet, so such a file is left whole.
    pub fn unlink(&mut self, parent: u64, name: &[u8]) -> Result<()> {
        self.remove_name(parent, name, false)
    }

    /// Remove the empty directory `name` from `parent`.
    ///
    /// # Errors
    ///
    /// As [`Filesystem::unlink`], with [`Error::NotADirectory`] for
    /// anything but a directory and [`Error::UnsupportedFeature`] for one
    /// that is not empty.
    pub fn rmdir(&mut self, parent: u64, name: &[u8]) -> Result<()> {
        self.remove_name(parent, name, true)
    }

    fn remove_name(&mut self, parent: u64, name: &[u8], want_dir: bool) -> Result<()> {
        self.require_writable_top_level()?;
        check_name(name)?;
        let dir = self.read_inode(parent)?;
        if !dir.is_dir() {
            return Err(Error::NotADirectory);
        }
        let entry = self.lookup_entry(parent, name)?;
        if !entry.is_inode() {
            return Err(Error::UnsupportedFeature(format!(
                "{:?} is subvolume {}, and deleting a subvolume is not this operation",
                String::from_utf8_lossy(name),
                entry.ino
            )));
        }
        let child = self.read_inode(entry.ino)?;
        match (want_dir, child.is_dir()) {
            (false, true) => return Err(Error::NotAFile),
            (true, false) => return Err(Error::NotADirectory),
            _ => {}
        }
        let ino = child.ino;
        if want_dir {
            let has_entries = self
                .last_key_at_or_before(&key(ino, DIR_INDEX_KEY, u64::MAX))?
                .is_some_and(|k| k.objectid == ino && k.key_type == DIR_INDEX_KEY);
            if child.size != 0 || has_entries {
                return Err(Error::UnsupportedFeature(format!(
                    "directory {:?} is not empty",
                    String::from_utf8_lossy(name)
                )));
            }
        }
        let generation = self.next_generation()?;
        let now = now();
        let mut edits = Vec::new();

        // The child's reference to this parent loses the name, and gives
        // up the index the parent filed it under.
        let ref_key = key(ino, INODE_REF_KEY, parent);
        let refs = self.item_bytes(ref_key)?.ok_or_else(|| {
            Error::UnsupportedFeature(format!(
                "inode {ino} has no reference item from directory {parent}; a name held in \
                 an extended reference is not removed"
            ))
        })?;
        let (index, rest) = without_ref(&refs, name).ok_or_else(|| {
            Error::UnsupportedFeature(format!(
                "inode {ino}'s reference from directory {parent} does not hold {:?}; a name \
                 held in an extended reference is not removed",
                String::from_utf8_lossy(name)
            ))
        })?;
        edits.push(put_or_delete(ref_key, rest));

        // The parent's two entries.
        let hash_key = key(parent, DIR_ITEM_KEY, dir::name_hash(name));
        let by_hash = self.item_bytes(hash_key)?.ok_or(Error::NotFound)?;
        let rest = without_dir_record(&by_hash, name).ok_or(Error::NotFound)?;
        edits.push(put_or_delete(hash_key, rest));
        let index_key = key(parent, DIR_INDEX_KEY, index);
        if self.item_bytes(index_key)?.is_none() {
            return Err(Error::UnsupportedFeature(format!(
                "directory {parent} has no index entry {index} for {:?}, so its two indexes \
                 already disagree",
                String::from_utf8_lossy(name)
            )));
        }
        edits.push(ItemEdit::Delete(index_key));
        let mut raw = self.raw_inode(parent)?;
        let size = le64(&raw, io::SIZE).saturating_sub(2 * name.len() as u64);
        raw[io::SIZE..io::SIZE + 8].copy_from_slice(&size.to_le_bytes());
        touch(&mut raw, generation, now, &[io::CTIME, io::MTIME]);
        edits.push(put(parent, INODE_ITEM_KEY, 0, raw));

        // The child: one name fewer, or gone.
        let nlink = if want_dir { 1 } else { child.nlink };
        if nlink > 1 {
            let mut raw = self.raw_inode(ino)?;
            raw[io::NLINK..io::NLINK + 4].copy_from_slice(&(nlink - 1).to_le_bytes());
            touch(&mut raw, generation, now, &[io::CTIME]);
            edits.push(put(ino, INODE_ITEM_KEY, 0, raw));
        } else {
            for (k, data) in self.items_of(ino)? {
                match k.key_type {
                    // Already edited above.
                    INODE_REF_KEY if k.offset == parent => {}
                    INODE_ITEM_KEY | dir::XATTR_ITEM_KEY => edits.push(ItemEdit::Delete(k)),
                    EXTENT_DATA_KEY if holds_no_extent(&data) => edits.push(ItemEdit::Delete(k)),
                    EXTENT_DATA_KEY => {
                        return Err(Error::UnsupportedFeature(format!(
                            "{:?} is the last name of inode {ino}, which holds data extents, \
                             and releasing them is not implemented yet",
                            String::from_utf8_lossy(name)
                        )))
                    }
                    other => {
                        return Err(Error::UnsupportedFeature(format!(
                            "inode {ino} owns an item of type {other} at {}, which removing it \
                             does not handle",
                            k.offset
                        )))
                    }
                }
            }
        }
        self.commit_edits(ino, edits, generation, now)
    }

    /// Every fs tree item under `ino`, in key order.
    fn items_of(&self, ino: u64) -> Result<Vec<(DiskKey, Vec<u8>)>> {
        let reader = self.pool_reader();
        let tree = reader.tree();
        let mut out = Vec::new();
        tree.for_each_from(
            self.fs_tree_root,
            &key(ino, 0, 0),
            &mut |k: &DiskKey, d: &[u8]| {
                if k.objectid != ino {
                    return Ok(false);
                }
                out.push((*k, d.to_vec()));
                Ok(true)
            },
        )?;
        Ok(out)
    }

    /// The new inode's items, its name in `parent`, and the commit.
    fn add_inode(
        &mut self,
        parent: u64,
        name: &[u8],
        kind: NewKind,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<u64> {
        self.require_writable_top_level()?;
        let dir = self.writable_parent(parent, name)?;
        if let NewKind::Symlink(target) = kind {
            let limit = (self.sb.sectorsize as usize - 1).min(self.max_item_size() - INLINE_HEADER);
            if target.is_empty() || target.len() > limit {
                return Err(Error::UnsupportedFeature(format!(
                    "a symbolic link's target of {} bytes: it must be 1 to {limit} bytes to be \
                     stored inline, and a symbolic link is never stored any other way",
                    target.len()
                )));
            }
        }
        let generation = self.next_generation()?;
        let now = now();
        let ino = self.next_objectid()?;
        let index = self.next_dir_index(parent)?;

        let (type_bits, ft, size) = match kind {
            NewKind::File => (S_IFREG, ftype::REG_FILE, 0),
            NewKind::Dir => (S_IFDIR, ftype::DIR, 0),
            NewKind::Symlink(t) => (S_IFLNK, ftype::SYMLINK, t.len() as u64),
        };
        // What a new inode takes from its directory: copy-on-write or
        // not, and the compression preference. A regular file that is
        // nodatacow is nodatasum too, since an extent overwritten in
        // place cannot keep a checksum.
        let mut flags = dir.flags & (INODE_NODATACOW | INODE_COMPRESS | INODE_NOCOMPRESS);
        match kind {
            NewKind::File if flags & INODE_NODATACOW != 0 => flags |= INODE_NODATASUM,
            NewKind::Symlink(_) => flags = 0,
            _ => {}
        }
        let raw = new_inode(NewInode {
            generation,
            size,
            mode: type_bits | (mode & 0o7777),
            uid,
            gid,
            flags,
            now,
        });

        let mut edits = vec![
            put(ino, INODE_ITEM_KEY, 0, raw),
            put(ino, INODE_REF_KEY, parent, inode_ref(index, name)),
        ];
        if let NewKind::Symlink(target) = kind {
            edits.push(put(
                ino,
                EXTENT_DATA_KEY,
                0,
                inline_extent(generation, target),
            ));
        }
        self.entry_edits(&mut edits, &dir, name, ino, ft, index, generation, now)?;
        self.commit_edits(ino, edits, generation, now)?;
        Ok(ino)
    }

    /// The parent's two entries for `name` and its own updated inode.
    #[allow(clippy::too_many_arguments)]
    fn entry_edits(
        &self,
        edits: &mut Vec<ItemEdit>,
        dir: &Inode,
        name: &[u8],
        child: u64,
        ft: u8,
        index: u64,
        generation: u64,
        now: (u64, u32),
    ) -> Result<()> {
        let parent = dir.ino;
        let entry = dir_entry(child, generation, ft, name);
        // A name whose hash another name already has joins that item.
        let hash_key = key(parent, DIR_ITEM_KEY, dir::name_hash(name));
        let mut by_hash = self.item_bytes(hash_key)?.unwrap_or_default();
        by_hash.extend_from_slice(&entry);
        if by_hash.len() > self.max_item_size() {
            return Err(Error::UnsupportedFeature(format!(
                "directory {parent}'s names sharing the hash of {:?} would outgrow a leaf",
                String::from_utf8_lossy(name)
            )));
        }
        edits.push(ItemEdit::Put(OwnedItem {
            key: hash_key,
            data: by_hash,
        }));
        edits.push(put(parent, DIR_INDEX_KEY, index, entry));

        // The directory's size counts each name once per entry.
        let mut raw = self.raw_inode(parent)?;
        let size = le64(&raw, io::SIZE) + 2 * name.len() as u64;
        raw[io::SIZE..io::SIZE + 8].copy_from_slice(&size.to_le_bytes());
        touch(&mut raw, generation, now, &[io::CTIME, io::MTIME]);
        edits.push(put(parent, INODE_ITEM_KEY, 0, raw));
        Ok(())
    }

    /// Plan, render and commit `edits` to the fs tree as one transaction,
    /// then reopen the mount on the generation just written.
    pub(crate) fn commit_edits(
        &mut self,
        ino: u64,
        edits: Vec<ItemEdit>,
        generation: u64,
        now: (u64, u32),
    ) -> Result<()> {
        let write = DataWrite {
            root: objectid::FS_TREE,
            ino,
            moves: Vec::new(),
            time: now,
            root_flags: None,
            default_subvol: None,
            edits,
        };
        let dirty: Vec<u64> = {
            let reader = self.pool_reader();
            let tree = reader.tree();
            let mut leaves = std::collections::BTreeSet::new();
            for edit in &write.edits {
                leaves.insert(tree.descend(self.fs_tree_root, &edit.key())?.header.bytenr);
            }
            leaves.into_iter().collect()
        };
        let plan = self.plan_transaction_closed_with(&dirty, &write, PLAN_ROUNDS)?;
        let blocks = self.render_plan_with(&plan, &write, generation)?;
        let root = self.planned_root(&plan).ok_or_else(|| {
            Error::UnsupportedFeature("the change's plan does not move the root tree".into())
        })?;
        let bytes_used = match plan.usage_delta(u64::from(self.sb.nodesize)) {
            0 => None,
            d => Some(
                u64::try_from(i128::from(self.sb.bytes_used) + d).map_err(|_| {
                    Error::UnsupportedFeature(format!(
                        "the superblock's {} bytes used cannot move by {d}",
                        self.sb.bytes_used
                    ))
                })?,
            ),
        };
        let device = Arc::clone(self.writable.as_ref().expect("checked by the caller"));
        self.commit(
            &blocks,
            &Commit {
                generation,
                root,
                bytes_used,
                invalidate_free_space_tree: false,
                ..Default::default()
            },
        )?;
        *self = Filesystem::mount_rw(device)?;
        Ok(())
    }

    /// Refuse a read-only mount, and one whose tree is not tree 5.
    pub(crate) fn require_writable_top_level(&self) -> Result<()> {
        if self.writable.is_none() {
            return Err(Error::ReadOnly);
        }
        let top = self.tree_root(objectid::FS_TREE)?;
        if top != self.fs_tree_root {
            return Err(Error::UnsupportedFeature(
                "this mount reads a subvolume other than the top level, and only the top \
                 level is written"
                    .into(),
            ));
        }
        Ok(())
    }

    /// `parent`, checked to be a directory with no entry `name`, and
    /// `name` checked to be one a directory can hold.
    fn writable_parent(&self, parent: u64, name: &[u8]) -> Result<Inode> {
        check_name(name)?;
        let dir = self.read_inode(parent)?;
        if !dir.is_dir() {
            return Err(Error::NotADirectory);
        }
        match self.lookup_entry(parent, name) {
            Ok(_) => Err(Error::UnsupportedFeature(format!(
                "{:?} already exists in directory {parent}",
                String::from_utf8_lossy(name)
            ))),
            Err(Error::NotFound) => Ok(dir),
            Err(e) => Err(e),
        }
    }

    /// The transaction id the change commits as.
    pub(crate) fn next_generation(&self) -> Result<u64> {
        self.sb
            .generation
            .checked_add(1)
            .ok_or_else(|| Error::UnsupportedFeature("the generation counter is exhausted".into()))
    }

    /// One past the highest inode number in the tree.
    fn next_objectid(&self) -> Result<u64> {
        let last = self.last_key_at_or_before(&key(LAST_FREE_OBJECTID, u8::MAX, u64::MAX))?;
        let highest = last.map_or(FIRST_FREE_OBJECTID, |k| k.objectid.max(FIRST_FREE_OBJECTID));
        if highest >= LAST_FREE_OBJECTID {
            return Err(Error::UnsupportedFeature(
                "the tree has no inode numbers left".into(),
            ));
        }
        Ok(highest + 1)
    }

    /// One past the highest `DIR_INDEX` in `dir`, or the first index.
    fn next_dir_index(&self, dir: u64) -> Result<u64> {
        let last = self.last_key_at_or_before(&key(dir, DIR_INDEX_KEY, u64::MAX))?;
        Ok(match last {
            Some(k) if k.objectid == dir && k.key_type == DIR_INDEX_KEY => k
                .offset
                .checked_add(1)
                .ok_or_else(|| {
                    Error::UnsupportedFeature(format!("directory {dir} has no indexes left"))
                })?
                .max(FIRST_DIR_INDEX),
            _ => FIRST_DIR_INDEX,
        })
    }

    /// The greatest key in the fs tree not after `target`.
    fn last_key_at_or_before(&self, target: &DiskKey) -> Result<Option<DiskKey>> {
        let reader = self.pool_reader();
        let tree = reader.tree();
        let leaf = tree.descend(self.fs_tree_root, target)?;
        let order = |k: &DiskKey| (k.objectid, k.key_type, k.offset);
        Ok(leaf
            .body
            .items()
            .unwrap_or(&[])
            .iter()
            .map(|i| i.key)
            .filter(|k| order(k) <= order(target))
            .max_by_key(order))
    }

    /// An fs tree item's bytes, if there is one under `key`.
    pub(crate) fn item_bytes(&self, key: DiskKey) -> Result<Option<Vec<u8>>> {
        let reader = self.pool_reader();
        let tree = reader.tree();
        Ok(tree.search(self.fs_tree_root, &key)?.map(|i| i.data))
    }

    /// An inode item's bytes, as stored.
    pub(crate) fn raw_inode(&self, ino: u64) -> Result<Vec<u8>> {
        let raw = self
            .item_bytes(key(ino, INODE_ITEM_KEY, 0))?
            .ok_or(Error::NotFound)?;
        if raw.len() < INODE_ITEM_SIZE {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino}'s item is {} bytes, shorter than an inode",
                raw.len()
            )));
        }
        Ok(raw)
    }

    /// The most one item can hold: a leaf's space less its header and
    /// one item header.
    pub(crate) fn max_item_size(&self) -> usize {
        self.sb.nodesize as usize - LEAF_HEADER - ITEM_HEADER
    }
}

/// A leaf's header, and the header of each item in it.
const LEAF_HEADER: usize = 101;
const ITEM_HEADER: usize = 25;

/// The header of an inline `EXTENT_DATA` item, before its data.
const INLINE_HEADER: usize = 21;

/// Whether `name` is one a directory can hold: 1 to 255 bytes, neither
/// `.` nor `..`, with no `/` and no NUL.
pub fn is_valid_name(name: &[u8]) -> bool {
    check_name(name).is_ok()
}

/// Refuse a name no directory may hold.
fn check_name(name: &[u8]) -> Result<()> {
    if name.is_empty()
        || name.len() > MAX_NAME_LEN
        || name == b"."
        || name == b".."
        || name.contains(&b'/')
        || name.contains(&0)
    {
        return Err(Error::UnsupportedFeature(format!(
            "{:?} is not a name a directory can hold: 1 to {MAX_NAME_LEN} bytes, neither `.` \
             nor `..`, with no `/` and no NUL",
            String::from_utf8_lossy(name)
        )));
    }
    Ok(())
}

/// The directory-entry type of an existing inode.
fn file_type(inode: &Inode) -> u8 {
    match inode.mode & S_IFMT {
        S_IFREG => ftype::REG_FILE,
        S_IFDIR => ftype::DIR,
        S_IFLNK => ftype::SYMLINK,
        0o020_000 => ftype::CHRDEV,
        0o060_000 => ftype::BLKDEV,
        0o010_000 => ftype::FIFO,
        0o140_000 => ftype::SOCK,
        _ => ftype::UNKNOWN,
    }
}

fn key(objectid: u64, key_type: u8, offset: u64) -> DiskKey {
    DiskKey {
        objectid,
        key_type,
        offset,
    }
}

fn put(objectid: u64, key_type: u8, offset: u64, data: Vec<u8>) -> ItemEdit {
    ItemEdit::Put(OwnedItem {
        key: key(objectid, key_type, offset),
        data,
    })
}

pub(crate) fn now() -> (u64, u32) {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (d.as_secs(), d.subsec_nanos())
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

fn put_time(b: &mut [u8], at: usize, t: (u64, u32)) {
    b[at..at + 8].copy_from_slice(&t.0.to_le_bytes());
    b[at + 8..at + 12].copy_from_slice(&t.1.to_le_bytes());
}

/// What the kernel changes on an inode it modifies: the transaction that
/// last touched it, its change counter, and the given times.
pub(crate) fn touch(raw: &mut [u8], generation: u64, now: (u64, u32), times: &[usize]) {
    raw[io::TRANSID..io::TRANSID + 8].copy_from_slice(&generation.to_le_bytes());
    let sequence = le64(raw, io::SEQUENCE).wrapping_add(1);
    raw[io::SEQUENCE..io::SEQUENCE + 8].copy_from_slice(&sequence.to_le_bytes());
    for &at in times {
        put_time(raw, at, now);
    }
}

/// The fields a new inode item is made from.
struct NewInode {
    generation: u64,
    size: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    flags: u64,
    now: (u64, u32),
}

/// A new `INODE_ITEM`: one link, every time now, and its data (if any)
/// counted in `nbytes`.
fn new_inode(n: NewInode) -> Vec<u8> {
    let mut b = vec![0u8; INODE_ITEM_SIZE];
    b[io::GENERATION..io::GENERATION + 8].copy_from_slice(&n.generation.to_le_bytes());
    b[io::TRANSID..io::TRANSID + 8].copy_from_slice(&n.generation.to_le_bytes());
    b[io::SIZE..io::SIZE + 8].copy_from_slice(&n.size.to_le_bytes());
    // Only a symbolic link has data here, and inline data counts in full.
    let nbytes = if n.mode & S_IFMT == S_IFLNK {
        n.size
    } else {
        0
    };
    b[io::NBYTES..io::NBYTES + 8].copy_from_slice(&nbytes.to_le_bytes());
    b[io::NLINK..io::NLINK + 4].copy_from_slice(&1u32.to_le_bytes());
    b[io::UID..io::UID + 4].copy_from_slice(&n.uid.to_le_bytes());
    b[io::GID..io::GID + 4].copy_from_slice(&n.gid.to_le_bytes());
    b[io::MODE..io::MODE + 4].copy_from_slice(&n.mode.to_le_bytes());
    b[io::FLAGS..io::FLAGS + 8].copy_from_slice(&n.flags.to_le_bytes());
    b[io::SEQUENCE..io::SEQUENCE + 8].copy_from_slice(&1u64.to_le_bytes());
    for at in [io::ATIME, io::CTIME, io::MTIME, io::OTIME] {
        put_time(&mut b, at, n.now);
    }
    b
}

/// Put what is left of an item, or delete it when nothing is.
pub(crate) fn put_or_delete(k: DiskKey, rest: Vec<u8>) -> ItemEdit {
    if rest.is_empty() {
        ItemEdit::Delete(k)
    } else {
        ItemEdit::Put(OwnedItem { key: k, data: rest })
    }
}

/// An `INODE_REF` item without the record naming `name`: that record's
/// index, and the bytes left. `None` when no record holds the name or
/// the records do not parse.
fn without_ref(data: &[u8], name: &[u8]) -> Option<(u64, Vec<u8>)> {
    let mut pos = 0;
    while pos + INODE_REF_HEADER <= data.len() {
        let len = usize::from(u16::from_le_bytes([data[pos + 8], data[pos + 9]]));
        let end = pos + INODE_REF_HEADER + len;
        if end > data.len() {
            return None;
        }
        if &data[pos + INODE_REF_HEADER..end] == name {
            let index = le64(data, pos);
            let mut rest = data[..pos].to_vec();
            rest.extend_from_slice(&data[end..]);
            return Some((index, rest));
        }
        pos = end;
    }
    None
}

/// A `DIR_ITEM` without the record naming `name`, or `None` when no
/// record holds it.
pub(crate) fn without_dir_record(data: &[u8], name: &[u8]) -> Option<Vec<u8>> {
    use dir::offsets as d;
    let mut pos = 0;
    while pos + d::NAME <= data.len() {
        let r = &data[pos..];
        let data_len = usize::from(u16::from_le_bytes([r[d::DATA_LEN], r[d::DATA_LEN + 1]]));
        let name_len = usize::from(u16::from_le_bytes([r[d::NAME_LEN], r[d::NAME_LEN + 1]]));
        let end = pos + d::NAME + name_len + data_len;
        if end > data.len() {
            return None;
        }
        if &r[d::NAME..d::NAME + name_len] == name {
            let mut rest = data[..pos].to_vec();
            rest.extend_from_slice(&data[end..]);
            return Some(rest);
        }
        pos = end;
    }
    None
}

/// Whether an `EXTENT_DATA` item references no extent: inline data, or
/// a hole (a regular item whose disk address is zero).
fn holds_no_extent(data: &[u8]) -> bool {
    const TYPE: usize = 20;
    const DISK_BYTENR: usize = 21;
    match data.get(TYPE) {
        Some(0) => true,
        Some(1 | 2) => data.len() >= DISK_BYTENR + 8 && le64(data, DISK_BYTENR) == 0,
        _ => false,
    }
}

/// One `INODE_REF` record: the name's index in the parent, and the name.
fn inode_ref(index: u64, name: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(INODE_REF_HEADER + name.len());
    b.extend_from_slice(&index.to_le_bytes());
    b.extend_from_slice(&(name.len() as u16).to_le_bytes());
    b.extend_from_slice(name);
    b
}

/// One directory entry record, as `DIR_ITEM` and `DIR_INDEX` both hold
/// it: the child's inode key, the transaction, no data, the name.
fn dir_entry(child: u64, generation: u64, ft: u8, name: &[u8]) -> Vec<u8> {
    use dir::offsets as d;
    let mut b = vec![0u8; d::NAME];
    b[d::LOCATION..d::LOCATION + 8].copy_from_slice(&child.to_le_bytes());
    b[d::LOCATION + 8] = INODE_ITEM_KEY;
    // The location's offset, zero, is already there.
    b[d::TRANSID..d::TRANSID + 8].copy_from_slice(&generation.to_le_bytes());
    b[d::NAME_LEN..d::NAME_LEN + 2].copy_from_slice(&(name.len() as u16).to_le_bytes());
    b[d::TYPE] = ft;
    b.extend_from_slice(name);
    b
}

/// An inline, uncompressed `EXTENT_DATA` item holding `data`.
fn inline_extent(generation: u64, data: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; INLINE_HEADER];
    b[0..8].copy_from_slice(&generation.to_le_bytes());
    b[8..16].copy_from_slice(&(data.len() as u64).to_le_bytes());
    // Compression, encryption, other encoding: none. Type 0: inline.
    b.extend_from_slice(data);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_refused_when_no_directory_could_hold_it() {
        for bad in [&b""[..], b".", b"..", b"a/b", b"nul\0", &[b'x'; 256]] {
            assert!(check_name(bad).is_err(), "{bad:?} was accepted");
        }
        assert!(check_name(&[b'x'; 255]).is_ok());
        assert!(check_name("caf\u{e9}".as_bytes()).is_ok());
    }

    #[test]
    fn a_directory_entry_record_parses_back_as_what_was_written() {
        let rec = dir_entry(300, 9, ftype::SYMLINK, b"link");
        let parsed = dir::parse_dir_items(&rec).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, b"link");
        assert_eq!(parsed[0].ino, 300);
        assert_eq!(parsed[0].transid, 9);
        assert_eq!(parsed[0].location_type, INODE_ITEM_KEY);
    }

    #[test]
    fn removing_one_record_keeps_the_others() {
        let mut item = dir_entry(300, 9, ftype::REG_FILE, b"a");
        item.extend(dir_entry(301, 9, ftype::REG_FILE, b"bb"));
        let rest = without_dir_record(&item, b"a").unwrap();
        assert_eq!(rest, dir_entry(301, 9, ftype::REG_FILE, b"bb"));
        assert!(without_dir_record(&rest, b"bb").unwrap().is_empty());
        assert!(without_dir_record(&rest, b"a").is_none());

        let mut refs = inode_ref(2, b"one");
        refs.extend(inode_ref(7, b"two"));
        let (index, rest) = without_ref(&refs, b"two").unwrap();
        assert_eq!(index, 7);
        assert_eq!(rest, inode_ref(2, b"one"));
        assert!(without_ref(&rest, b"two").is_none());
    }

    #[test]
    fn a_new_inode_parses_back_as_what_was_written() {
        let raw = new_inode(NewInode {
            generation: 7,
            size: 5,
            mode: S_IFLNK | 0o777,
            uid: 1000,
            gid: 100,
            flags: 0,
            now: (1_700_000_000, 5),
        });
        let inode = Inode::parse(&raw, 260).unwrap();
        assert!(inode.is_symlink());
        assert_eq!(inode.size, 5);
        assert_eq!(inode.nlink, 1);
        assert_eq!((inode.uid, inode.gid), (1000, 100));
    }
}
