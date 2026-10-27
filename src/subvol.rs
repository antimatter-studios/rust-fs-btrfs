//! Subvolumes and snapshots.
//!
//! Every fixture before this one had exactly one subvolume — the default
//! fs tree, objectid 5 — which is the only shape the driver had ever
//! been asked to read and is not the shape a real Btrfs filesystem has.
//! Subvolumes are how people use Btrfs, and a snapshot is how they back
//! it up.
//!
//! # Where they live
//!
//! The root tree is the index of trees. Walking it gives three kinds of
//! item that matter here, and what each one is was read off a filesystem
//! rather than assumed:
//!
//! ```text
//! type 132  ROOT_ITEM      one per tree: where its root block is, its
//!                          generation, its flags
//! type 156  ROOT_REF       key (parent, 156, child), data ends in the name
//! type 144  ROOT_BACKREF   key (child, 144, parent), the same name
//! ```
//!
//! `ROOT_BACKREF` is the one used here: one item per subvolume giving
//! its parent, the directory of the parent its entry sits in, and its
//! name, which is exactly what building a path needs. `ROOT_REF` says
//! the same thing from the other end.
//!
//! The directory matters. A subvolume made inside an ordinary directory
//! of its parent, rather than in the parent's top directory, is listed by
//! `btrfs subvolume list` with that directory in its path: `top/nested`,
//! not `nested` (#362). The directory's own path is read from the
//! parent's tree, one `INODE_REF` at a time up to its top directory.
//!
//! # Four things that were measured
//!
//! **A snapshot is a `ROOT_ITEM` whose key offset is not zero.** On a
//! filesystem with two subvolumes and two snapshots, the two snapshots
//! read 10 and 11 — the generation each was created at — and everything
//! else read 0.
//!
//! **Read-only is `flags` bit 0**, at offset 208 in the item. Of five
//! subvolumes on that filesystem, exactly the one created with
//! `btrfs subvolume snapshot -r` had it set.
//!
//! **The name is 18 bytes into the reference item.** A `dirid` and a
//! `sequence`, both 64-bit, then a 16-bit length. Confirmed on four
//! names of different lengths: `sub` in a 21-byte item, `snap` in 22,
//! `inner` in 23, `rosnap` in 24.
//!
//! **Some objectids are negative.** The root tree of that same
//! filesystem holds one numbered 18446744073709551607, which is −9 read
//! as unsigned. A filter of "256 or above" admits it; the range has an
//! upper end as well as a lower one, and [`LAST_FREE_OBJECTID`] is it.

use crate::error::{Error, Result};
use crate::fs::Filesystem;
use crate::super_write::Commit;
use crate::transaction::DataWrite;

use crate::fs::ROOT_ITEM_KEY;

/// `BTRFS_ROOT_BACKREF_KEY` — a subvolume's own record of its parent and
/// its name.
const ROOT_BACKREF_KEY: u8 = 144;

/// `BTRFS_FS_TREE_OBJECTID` — the default subvolume, which every
/// filesystem has and which has no name and no parent.
pub use crate::chunk::objectid::FS_TREE as FS_TREE_OBJECTID;

/// `BTRFS_FIRST_FREE_OBJECTID` — the lowest id a created subvolume gets.
pub use crate::inode::FIRST_FREE_OBJECTID;

/// `BTRFS_LAST_FREE_OBJECTID` — the highest, and the reason this range
/// has two ends.
///
/// Btrfs numbers several internal trees with *negative* objectids, which
/// read as very large unsigned values. One such tree sits in the root
/// tree of an ordinary filesystem, so "256 or above" is not a filter,
/// it is a filter that admits an internal tree and reports it as a
/// subvolume.
pub const LAST_FREE_OBJECTID: u64 = u64::MAX - 255;

/// Byte offsets within `struct btrfs_root_item`.
///
/// The item opens with an embedded 160-byte `btrfs_inode_item`, then a
/// generation and a `root_dirid` before the address of the root block.
use crate::fs::root_item;

/// Byte offsets within `struct btrfs_root_ref`, which both the reference
/// and the back-reference use.
mod root_ref {
    /// `u64`. The directory of the parent the subvolume's entry is in.
    pub const DIRID: usize = 0;
    /// `u64`. The index the parent files the subvolume's entry under.
    pub const SEQUENCE: usize = 8;
    /// `u16`. How many bytes of name follow.
    pub const NAME_LEN: usize = 16;
    /// The name itself.
    pub const NAME: usize = 18;
}

/// `BTRFS_ROOT_SUBVOL_RDONLY`.
const ROOT_SUBVOL_RDONLY: u64 = 1 << 0;

/// `ROOT_REF`: filed in the root tree under the parent tree's id, with the
/// child's id as the offset, saying which directory of the parent holds
/// the child and by what name.
const ROOT_REF_KEY: u8 = 156;

/// `BTRFS_EMPTY_SUBVOL_DIR_OBJECTID`: the inode number the kernel gives
/// the empty directory it shows where a subvolume entry has no
/// `ROOT_REF` behind it. That is what a snapshot's copy of a nested
/// subvolume's entry looks like, because a snapshot does not carry the
/// subvolumes nested in its source. No tree holds an inode with this
/// number.
pub const EMPTY_SUBVOL_DIR_OBJECTID: u64 = 2;

/// Where a path ends: an inode, and the tree it is an inode of.
pub struct PathTarget {
    /// The subvolume the path crossed into last, or `None` when it
    /// crossed none and the inode belongs to the handle it started from.
    pub tree: Option<Filesystem>,
    /// The inode, meaningful only in [`PathTarget::fs`].
    pub inode: crate::inode::Inode,
}

impl PathTarget {
    /// The handle to read [`inode`](Self::inode) through, given the one
    /// the path was resolved from.
    pub fn fs<'a>(&'a self, start: &'a Filesystem) -> &'a Filesystem {
        self.tree.as_ref().unwrap_or(start)
    }
}

/// One subvolume or snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subvolume {
    /// Its objectid, which is what `btrfs subvolume list` calls the ID.
    pub id: u64,
    /// The subvolume it was created inside, or zero for the default one.
    pub parent: u64,
    /// Its name within its parent. Empty for the default subvolume,
    /// which has none.
    pub name: Vec<u8>,
    /// Its path from the top of the filesystem, as
    /// `btrfs subvolume list` prints it — `sub/inner` rather than
    /// `inner`.
    pub path: String,
    /// The root block of its tree.
    pub bytenr: u64,
    /// The transaction it was last written in.
    pub generation: u64,
    /// The generation it was last snapshotted at, or zero.
    pub last_snapshot: u64,
    /// Whether it was created read-only.
    pub read_only: bool,
    /// Whether it is a snapshot of another subvolume rather than a
    /// subvolume created empty.
    pub is_snapshot: bool,
}

impl Subvolume {
    /// Whether this is the default subvolume, which is present on every
    /// filesystem and is nobody's child.
    pub fn is_default(&self) -> bool {
        self.id == FS_TREE_OBJECTID
    }
}

/// Whether an objectid names a subvolume rather than one of the
/// filesystem's internal trees.
///
/// The default subvolume, or one in the free range — which has an upper
/// end because some internal trees are numbered negatively and read as
/// enormous unsigned values.
pub fn is_subvolume_id(objectid: u64) -> bool {
    objectid == FS_TREE_OBJECTID || (FIRST_FREE_OBJECTID..=LAST_FREE_OBJECTID).contains(&objectid)
}

impl Filesystem {
    /// Every subvolume and snapshot on the filesystem, by ascending id.
    ///
    /// The default subvolume comes first, as id 5, with an empty name
    /// and a path of `/`. The rest carry the path
    /// `btrfs subvolume list` would print.
    ///
    /// # Errors
    ///
    /// As the B-tree walk.
    pub fn subvolumes(&self) -> Result<Vec<Subvolume>> {
        let items = self.root_tree_items()?;

        // Two passes, because a path cannot be built until every name is
        // known: a nested subvolume names only its immediate parent, and
        // the parent may be read after the child.
        let mut names: std::collections::BTreeMap<u64, (u64, Vec<u8>)> =
            std::collections::BTreeMap::new();
        // Each subvolume's link in its path: the directory its entry sits
        // in, from the parent's top directory, then its name.
        let mut links: std::collections::BTreeMap<u64, (u64, Vec<u8>)> =
            std::collections::BTreeMap::new();
        let mut roots: std::collections::BTreeMap<u64, u64> = std::collections::BTreeMap::new();
        let le64 = |data: &[u8], at: usize| {
            u64::from_le_bytes(data[at..at + 8].try_into().expect("8 bytes"))
        };
        for (objectid, key_type, _, data) in &items {
            if *key_type == ROOT_ITEM_KEY
                && is_subvolume_id(*objectid)
                && data.len() >= root_item::MIN_SIZE
            {
                roots.insert(*objectid, le64(data, root_item::BYTENR));
            }
        }
        // Each parent's tree is opened once, and only when an entry sits
        // somewhere other than its top directory.
        let mut trees: std::collections::BTreeMap<u64, Filesystem> =
            std::collections::BTreeMap::new();
        for (objectid, key_type, offset, data) in &items {
            if *key_type != ROOT_BACKREF_KEY || !is_subvolume_id(*objectid) {
                continue;
            }
            let Some(name) = reference_name(data) else {
                continue;
            };
            let dirid = le64(data, root_ref::DIRID);
            let mut link = Vec::new();
            if dirid != FIRST_FREE_OBJECTID {
                let tree = match trees.entry(*offset) {
                    std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::btree_map::Entry::Vacant(e) => {
                        let bytenr = roots.get(offset).ok_or_else(|| {
                            Error::UnsupportedFeature(format!(
                                "subvolume {objectid} names subvolume {offset} as its \
                                 parent, which has no root item"
                            ))
                        })?;
                        e.insert(self.reroot(*bytenr)?)
                    }
                };
                link = tree.directory_path(dirid)?;
                link.push(b'/');
            }
            link.extend_from_slice(&name);
            links.insert(*objectid, (*offset, link));
            names.insert(*objectid, (*offset, name));
        }

        let mut out = Vec::new();
        for (objectid, key_type, offset, data) in &items {
            if *key_type != ROOT_ITEM_KEY
                || !is_subvolume_id(*objectid)
                || data.len() < root_item::MIN_SIZE
            {
                continue;
            }
            // A deleted subvolume no directory refers to any more, waiting
            // for the kernel's cleaner: `btrfs subvolume list` leaves it out.
            if *objectid != FS_TREE_OBJECTID
                && data
                    .get(root_item_more::REFS..root_item_more::REFS + 4)
                    .is_some_and(|r| r == [0; 4])
            {
                continue;
            }
            let (parent, name) = names.get(objectid).cloned().unwrap_or((0, Vec::new()));

            out.push(Subvolume {
                id: *objectid,
                parent,
                path: path_of(*objectid, &links),
                name,
                bytenr: le64(data, root_item::BYTENR),
                generation: le64(data, root_item::GENERATION),
                last_snapshot: le64(data, root_item::LAST_SNAPSHOT),
                read_only: le64(data, root_item::FLAGS) & ROOT_SUBVOL_RDONLY != 0,
                // A snapshot records the generation it was taken at in
                // its key's offset; a subvolume created empty has zero.
                is_snapshot: *offset != 0,
            });
        }

        out.sort_by_key(|s| s.id);
        Ok(out)
    }
}

/// The name out of a `ROOT_REF` or `ROOT_BACKREF` item.
///
/// `None` rather than an error for an item too short to hold what it
/// claims: a listing that skipped one malformed reference is more useful
/// than one that refused to produce anything.
fn reference_name(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < root_ref::NAME {
        return None;
    }
    let len = u16::from_le_bytes(
        data[root_ref::NAME_LEN..root_ref::NAME_LEN + 2]
            .try_into()
            .expect("2 bytes"),
    ) as usize;
    let end = root_ref::NAME.checked_add(len)?;
    if end > data.len() {
        return None;
    }
    Some(data[root_ref::NAME..end].to_vec())
}

/// The name in the first record of an `INODE_REF` item: an 8-byte index,
/// a 2-byte length, then the name. A directory has exactly one record,
/// because a directory cannot be hard-linked.
fn inode_ref_name(data: &[u8]) -> Option<Vec<u8>> {
    let len = usize::from(u16::from_le_bytes(data.get(8..10)?.try_into().ok()?));
    Some(data.get(10..10 + len)?.to_vec())
}

/// The path a subvolume is reached by, built from its chain of parents.
///
/// Each entry of `names` is a subvolume's parent and its link in the path:
/// its name, after the directory of the parent it sits in when that is not
/// the parent's top directory.
///
/// Bounded rather than recursive: a filesystem whose references form a
/// cycle would otherwise hang here, and a listing is exactly the tool
/// someone reaches for when they suspect something is wrong.
fn path_of(id: u64, names: &std::collections::BTreeMap<u64, (u64, Vec<u8>)>) -> String {
    if id == FS_TREE_OBJECTID {
        return "/".to_string();
    }

    let mut parts: Vec<String> = Vec::new();
    let mut at = id;
    for _ in 0..names.len() + 1 {
        let Some((parent, name)) = names.get(&at) else {
            break;
        };
        parts.push(String::from_utf8_lossy(name).into_owned());
        if *parent == FS_TREE_OBJECTID {
            break;
        }
        at = *parent;
    }
    parts.reverse();
    parts.join("/")
}

impl Filesystem {
    /// The path of directory `dir` from this tree's top directory: the
    /// name in each directory's one `INODE_REF`, up to the top directory,
    /// whose own path is empty.
    ///
    /// # Errors
    ///
    /// A directory with no `INODE_REF`, or whose references never reach
    /// the top directory, is refused rather than listed under a partial
    /// path. Otherwise as the tree walk.
    fn directory_path(&self, dir: u64) -> Result<Vec<u8>> {
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let mut at = dir;
        // A tree deeper than this is a loop, not a directory hierarchy.
        for _ in 0..65_536 {
            if at == FIRST_FREE_OBJECTID {
                parts.reverse();
                return Ok(parts.join(&b'/'));
            }
            let ((_, _, parent), data) = self
                .item_run(at, crate::inode::INODE_REF_KEY)?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    Error::UnsupportedFeature(format!(
                        "directory {at} has no reference to its parent"
                    ))
                })?;
            if parent == at {
                break;
            }
            parts.push(inode_ref_name(&data).ok_or_else(|| {
                Error::UnsupportedFeature(format!(
                    "directory {at}'s reference to its parent is {} bytes, too short \
                     for the name it holds",
                    data.len()
                ))
            })?);
            at = parent;
        }
        Err(Error::UnsupportedFeature(format!(
            "directory {dir}'s parents do not reach the top directory"
        )))
    }

    /// Open one subvolume as a filesystem of its own.
    ///
    /// A subvolume is a separate tree over the same device. Everything
    /// below the tree — the chunk map, the superblock, the device — is
    /// shared; only which root the item cache is loaded from differs. So
    /// the returned handle answers `read_dir`, `lookup`, `read_file` and
    /// the rest against that subvolume, and paths inside it are absolute
    /// within it rather than carrying its own path as a prefix.
    ///
    /// # Read-only, except for what commits against its own tree
    ///
    /// The handle is read-only to every operation that only knows how
    /// to write the top level: [`Filesystem::is_writable`] is false, and
    /// those refuse it as [`crate::error::Error::ReadOnly`]. On a writable
    /// mount it holds the devices aside for the operations that name the
    /// subvolume's tree in their transaction: [`Filesystem::create`],
    /// [`Filesystem::mkdir`], [`Filesystem::symlink`], [`Filesystem::link`],
    /// [`Filesystem::unlink`], [`Filesystem::rmdir`], [`Filesystem::rename`],
    /// [`Filesystem::truncate`] and copy-on-write [`Filesystem::write`]. After
    /// each commit it is this subvolume's handle on the new generation.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::NotFound`] if no subvolume has that id, and whatever the
    /// tree walk returns.
    pub fn open_subvolume(&self, id: u64) -> Result<Filesystem> {
        let subvol = self
            .subvolumes()?
            .into_iter()
            .find(|s| s.id == id)
            .ok_or(crate::error::Error::NotFound)?;
        let mut fs = self.open_subvolume_at(subvol.bytenr)?;
        fs.subvolume_writes = self.writes_for_subvolume(id);
        Ok(fs)
    }

    /// Open the subvolume whose tree root is `bytenr`.
    ///
    /// Separate from [`Filesystem::open_subvolume`] so a caller that has
    /// already listed them does not pay for a second walk of the root
    /// tree to look up something it is holding.
    ///
    /// # Errors
    ///
    /// As the tree walk. A `bytenr` that is not a tree root fails there
    /// rather than producing an empty filesystem, because a tree block
    /// carries its own identity and the walk checks it.
    pub fn open_subvolume_at(&self, bytenr: u64) -> Result<Filesystem> {
        self.reroot(bytenr)
    }

    /// Resolve an absolute path to its inode, crossing into every
    /// subvolume on the way, and hand back the tree the inode belongs to.
    ///
    /// A crossing follows the kernel's `fixup_tree_root_location`. The
    /// entry names a subvolume, and a `ROOT_REF` from this tree must name
    /// the same directory and name. Then the path continues at that
    /// subvolume's top directory. Without a `ROOT_REF` the kernel shows an
    /// empty directory, inode [`EMPTY_SUBVOL_DIR_OBJECTID`], and so does
    /// this; nothing is reachable beneath it.
    ///
    /// Symbolic links are not followed, as in
    /// [`lookup_path`](Filesystem::lookup_path). Every crossed-into handle
    /// is read-only; see [`open_subvolume`](Filesystem::open_subvolume).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for a missing component,
    /// [`Error::NotADirectory`] for a component under a non-directory,
    /// and whatever the tree walks return.
    ///
    /// [`Error::NotFound`]: crate::error::Error::NotFound
    /// [`Error::NotADirectory`]: crate::error::Error::NotADirectory
    pub fn resolve_path(&self, path: &str) -> Result<PathTarget> {
        self.resolve_path_bytes(path.as_bytes())
    }

    /// [`resolve_path`] with the path given as bytes.
    ///
    /// The real one; the `&str` form above is the convenience. Btrfs
    /// directory entry names are raw bytes with no encoding rule, so a
    /// component is compared byte for byte against the on-disk entry and
    /// nothing is decoded on the way (#214).
    ///
    /// [`resolve_path`]: Self::resolve_path
    pub fn resolve_path_bytes(&self, path: &[u8]) -> Result<PathTarget> {
        use crate::error::Error;
        // Every subvolume crossed into, and the walk so far as (the tree
        // it is in, the directory). `..` pops the walk, which is how
        // climbing out of a subvolume's top lands back in the parent
        // tree's directory, as it does on a mount (#271). The trees stay
        // in `trees` until the end because a later step may climb back
        // into one already opened.
        let mut trees: Vec<Option<Filesystem>> = Vec::new();
        let mut walked: Vec<(Option<usize>, crate::inode::Inode)> =
            vec![(None, self.root_inode()?)];
        for component in path
            .split(|&b| b == b'/')
            .filter(|c| !c.is_empty() && *c != b".")
        {
            let (tree, inode) = walked.last().expect("the top is never popped").clone();
            if !inode.is_dir() {
                return Err(Error::NotADirectory);
            }
            if component == b".." {
                if walked.len() > 1 {
                    walked.pop();
                }
                continue;
            }
            if inode.ino == EMPTY_SUBVOL_DIR_OBJECTID {
                return Err(Error::NotFound);
            }
            let here = match tree {
                Some(i) => trees[i].as_ref().expect("taken only at the end"),
                None => self,
            };
            let entry = here.lookup_entry(inode.ino, component)?;
            if entry.is_inode() {
                let next = here.read_inode(entry.ino)?;
                walked.push((tree, next));
                continue;
            }
            match here.subvolume_behind(inode.ino, &entry)? {
                Some((bytenr, dirid)) => {
                    let child = here.reroot(bytenr)?;
                    let top = child.read_inode(dirid)?;
                    trees.push(Some(child));
                    walked.push((Some(trees.len() - 1), top));
                }
                None => walked.push((tree, empty_subvolume_dir(&inode))),
            }
        }
        let (tree, inode) = walked.pop().expect("the top is never popped");
        let tree = tree.map(|i| trees[i].take().expect("taken once"));
        Ok(PathTarget { tree, inode })
    }

    /// The root block and top directory of the subvolume `entry` names,
    /// when a `ROOT_REF` from this tree backs it.
    ///
    /// This handle knows its tree by root block, not by id, so the parent
    /// id is found as the tree whose `ROOT_ITEM` points at that block.
    fn subvolume_behind(
        &self,
        dir_ino: u64,
        entry: &crate::dir::DirEntry,
    ) -> Result<Option<(u64, u64)>> {
        use crate::error::Error;
        let reader = self.pool_reader();
        let root_tree = reader.tree();
        let root_item_of = |id: u64| -> Result<Option<Vec<u8>>> {
            Ok(root_tree
                .find_all(self.sb.root, id, ROOT_ITEM_KEY)?
                .into_iter()
                .next()
                .map(|item| item.data))
        };
        let Some(child) = root_item_of(entry.ino)? else {
            return Ok(None);
        };
        if child.len() < root_item::MIN_SIZE {
            return Err(Error::BadSuperblock(format!(
                "subvolume {}: ROOT_ITEM is {} bytes",
                entry.ino,
                child.len()
            )));
        }
        let le64 = |data: &[u8], at: usize| {
            u64::from_le_bytes(data[at..at + 8].try_into().expect("8 bytes"))
        };
        for backref in root_tree.find_all(self.sb.root, entry.ino, ROOT_BACKREF_KEY)? {
            let parent = backref.key.offset;
            let same_tree = root_item_of(parent)?.is_some_and(|data| {
                data.len() >= root_item::MIN_SIZE
                    && le64(&data, root_item::BYTENR) == self.fs_tree_root
            });
            if !same_tree {
                continue;
            }
            let backed = root_tree
                .search(
                    self.sb.root,
                    &crate::chunk::DiskKey {
                        objectid: parent,
                        key_type: ROOT_REF_KEY,
                        offset: entry.ino,
                    },
                )?
                .is_some_and(|item| {
                    item.data.len() >= 8
                        && le64(&item.data, 0) == dir_ino
                        && reference_name(&item.data).as_deref() == Some(&entry.name[..])
                });
            if backed {
                return Ok(Some((
                    le64(&child, root_item::BYTENR),
                    le64(&child, root_item::ROOT_DIRID),
                )));
            }
        }
        Ok(None)
    }
}

/// The directory the kernel shows for a subvolume entry with no
/// `ROOT_REF` behind it (`new_simple_dir`): mode 0755, one link, and every
/// time taken from the containing directory's change time.
fn empty_subvolume_dir(dir: &crate::inode::Inode) -> crate::inode::Inode {
    crate::inode::Inode {
        ino: EMPTY_SUBVOL_DIR_OBJECTID,
        size: 0,
        nbytes: 0,
        nlink: 1,
        uid: 0,
        gid: 0,
        mode: 0o040755,
        rdev: 0,
        flags: 0,
        atime: dir.ctime,
        mtime: dir.ctime,
        ctime: dir.ctime,
        otime: dir.ctime,
        ..dir.clone()
    }
}

/// How many rounds a flag change's plan may take to close over its own
/// bookkeeping, as for a file write.
const PLAN_ROUNDS: usize = 64;

/// Where a `ROOT_ITEM`'s received UUID is: after `generation_v2`, the
/// subvolume's own UUID and its parent's.
const ROOT_ITEM_RECEIVED_UUID: usize = root_item::GENERATION_V2 + 8 + 2 * 16;

impl Filesystem {
    /// Make subvolume `id` read-only, or writable again, and commit it.
    ///
    /// What `btrfs property set <subvol> ro true|false` does: bit 0 of the
    /// `ROOT_ITEM`'s flags, in a root tree leaf rewritten copy-on-write as
    /// one transaction. Nothing inside the subvolume changes. Asking for
    /// the state it is already in commits nothing.
    ///
    /// A received subvolume (one `btrfs receive` made, carrying a received
    /// UUID) is refused when made writable: an incremental stream applied
    /// to it later trusts that it has not changed since it was received,
    /// and the kernel clears the received UUID to keep that true, which
    /// this does not do yet.
    ///
    /// Takes `&mut self` because a commit moves the trees this mount reads;
    /// the mount is reopened on the new generation, as after
    /// [`Filesystem::write`].
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless mounted with [`Filesystem::mount_rw`],
    /// [`Error::NotFound`] for an id no subvolume has, and
    /// [`Error::UnsupportedFeature`] for a received subvolume made writable
    /// or a transaction the planner refuses.
    pub fn set_subvolume_read_only(&mut self, id: u64, read_only: bool) -> Result<()> {
        if self.writable.is_none() {
            return Err(Error::ReadOnly);
        }
        if !is_subvolume_id(id) {
            return Err(Error::NotFound);
        }
        let item = self
            .root_tree_items()?
            .into_iter()
            .find(|(objectid, key_type, _, _)| *objectid == id && *key_type == ROOT_ITEM_KEY)
            .map(|(_, _, _, data)| data)
            .ok_or(Error::NotFound)?;
        if item.len() < root_item::MIN_SIZE {
            return Err(Error::UnsupportedFeature(format!(
                "subvolume {id}'s root item is {} bytes, too short to hold its flags",
                item.len()
            )));
        }
        let flags = u64::from_le_bytes(
            item[root_item::FLAGS..root_item::FLAGS + 8]
                .try_into()
                .expect("8 bytes"),
        );
        let wanted = if read_only {
            flags | ROOT_SUBVOL_RDONLY
        } else {
            flags & !ROOT_SUBVOL_RDONLY
        };
        if wanted == flags {
            return Ok(());
        }
        let received = item
            .get(ROOT_ITEM_RECEIVED_UUID..ROOT_ITEM_RECEIVED_UUID + 16)
            .is_some_and(|u| u.iter().any(|&b| b != 0));
        if !read_only && received {
            return Err(Error::UnsupportedFeature(format!(
                "subvolume {id} was received from a send stream, and making it writable \
                 means clearing its received UUID, which is not implemented"
            )));
        }

        let leaf = self.root_item_leaf(id)?.ok_or(Error::NotFound)?;
        self.commit_root_tree_change(
            leaf,
            DataWrite {
                root_flags: Some((id, wanted)),
                ..Default::default()
            },
            0,
        )
    }

    /// Make subvolume `id` the one a mount without `subvol=` shows, and
    /// commit it.
    ///
    /// What `btrfs subvolume set-default` does: the root tree's directory
    /// (objectid 6) holds one entry, `default`, whose location names a
    /// subvolume's `ROOT_ITEM`. That entry is pointed at `id`, in place,
    /// and -- unless `id` is the top-level subvolume, which every kernel
    /// mounts by default anyway -- the superblock gains the
    /// `DEFAULT_SUBVOL` incompatible feature, as the kernel sets it, so a
    /// kernel that does not know the entry refuses the volume rather than
    /// mounting the wrong tree. Naming the subvolume that is already the
    /// default commits nothing.
    ///
    /// This handle's own reads still start at the top-level subvolume;
    /// the default is what a kernel mount shows.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless mounted with [`Filesystem::mount_rw`],
    /// [`Error::NotFound`] for an id no subvolume has or a root tree with
    /// no `default` entry, and [`Error::UnsupportedFeature`] for an entry
    /// that is not the shape the kernel writes or a transaction the planner
    /// refuses.
    pub fn set_default_subvolume(&mut self, id: u64) -> Result<()> {
        use crate::dir::{name_hash, DIR_ITEM_KEY};
        use crate::superblock::incompat::DEFAULT_SUBVOL;
        use crate::transaction::ROOT_TREE_DIR_OBJECTID;
        if self.writable.is_none() {
            return Err(Error::ReadOnly);
        }
        if !self.subvolumes()?.iter().any(|s| s.id == id) {
            return Err(Error::NotFound);
        }
        let key = (ROOT_TREE_DIR_OBJECTID, DIR_ITEM_KEY, name_hash(b"default"));
        let entry = self
            .root_tree_items()?
            .into_iter()
            .find(|(o, t, off, _)| (*o, *t, *off) == key)
            .map(|(_, _, _, data)| data)
            .ok_or(Error::NotFound)?;
        let current = entry
            .get(..8)
            .map(|b| u64::from_le_bytes(b.try_into().expect("8 bytes")));
        let incompat = if id == FS_TREE_OBJECTID {
            0
        } else {
            DEFAULT_SUBVOL
        };
        if current == Some(id) && self.sb.incompat_flags & incompat == incompat {
            return Ok(());
        }
        let leaf = {
            let reader = self.pool_reader();
            let tree = reader.tree();
            let key = crate::chunk::DiskKey {
                objectid: key.0,
                key_type: key.1,
                offset: key.2,
            };
            tree.descend(self.sb.root, &key)?.header.bytenr
        };
        self.commit_root_tree_change(
            leaf,
            DataWrite {
                default_subvol: Some(id),
                ..Default::default()
            },
            incompat,
        )
    }

    /// The subvolume a mount without `subvol=` shows: what `btrfs subvolume
    /// get-default` prints.
    ///
    /// Read from the `default` entry in the root tree's directory (objectid
    /// 6), the one [`Filesystem::set_default_subvolume`] rewrites. A root
    /// tree with no such entry has the top level as its default, as the
    /// kernel reads it.
    ///
    /// # Errors
    ///
    /// [`Error::BadSuperblock`] for an entry too short to name a subvolume,
    /// and whatever reading the root tree returns.
    pub fn default_subvolume(&self) -> Result<u64> {
        use crate::dir::{name_hash, DIR_ITEM_KEY};
        use crate::transaction::ROOT_TREE_DIR_OBJECTID;
        let key = (ROOT_TREE_DIR_OBJECTID, DIR_ITEM_KEY, name_hash(b"default"));
        let Some(entry) = self
            .root_tree_items()?
            .into_iter()
            .find(|(o, t, off, _)| (*o, *t, *off) == key)
            .map(|(_, _, _, data)| data)
        else {
            return Ok(FS_TREE_OBJECTID);
        };
        entry
            .get(..8)
            .map(|b| u64::from_le_bytes(b.try_into().expect("8 bytes")))
            .ok_or_else(|| {
                Error::BadSuperblock(format!(
                    "the root tree's default entry is {} bytes, too short to name a subvolume",
                    entry.len()
                ))
            })
    }

    /// The id of the subvolume whose top directory `path` names, from this
    /// handle's own top: `/` for the tree this handle reads, `/sub` for a
    /// subvolume called `sub` there, `/sub/inner` for one nested in it.
    ///
    /// The path is resolved as [`Filesystem::resolve_path_bytes`] resolves
    /// it, crossing into subvolumes on the way, so it names the subvolume
    /// the way a mounted filesystem's path would.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for a path that does not exist, or that ends
    /// anywhere but at a subvolume's top directory -- an ordinary
    /// directory, a file, or the empty directory a snapshot shows where its
    /// source had a nested subvolume -- and whatever the tree walks return.
    pub fn subvolume_at(&self, path: &[u8]) -> Result<u64> {
        let target = self.resolve_path_bytes(path)?;
        if target.inode.ino != FIRST_FREE_OBJECTID || !target.inode.is_dir() {
            return Err(Error::NotFound);
        }
        let root = target
            .tree
            .as_ref()
            .map_or(self.fs_tree_root, |t| t.fs_tree_root);
        self.subvolumes()?
            .into_iter()
            .find(|s| s.bytenr == root)
            .map(|s| s.id)
            .ok_or(Error::NotFound)
    }

    /// Commit a change to one root tree leaf as a transaction, then reopen
    /// the mount on the new generation.
    fn commit_root_tree_change(
        &mut self,
        leaf: u64,
        mut change: DataWrite,
        incompat: u64,
    ) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        change.root = FS_TREE_OBJECTID;
        change.time = (now.as_secs(), now.subsec_nanos());
        let generation = self.sb.generation.checked_add(1).ok_or_else(|| {
            Error::UnsupportedFeature("the generation counter is exhausted".into())
        })?;
        let plan = self.plan_transaction_closed_with(&[leaf], &change, PLAN_ROUNDS)?;
        let blocks = self.render_plan_with(&plan, &change, generation)?;
        let root = self.planned_root(&plan).ok_or_else(|| {
            Error::UnsupportedFeature("the change's plan does not move the root tree".into())
        })?;
        let delta = plan.usage_delta(u64::from(self.sb.nodesize));
        let bytes_used = match delta {
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
        self.commit_setting_incompat(
            &blocks,
            &Commit {
                generation,
                root,
                bytes_used,
                invalidate_free_space_tree: false,
                ..Default::default()
            },
            incompat,
        )?;
        // Every member of a pool, as the copy-on-write write reopens it
        // (#298): `mount_rw` opens one device and refuses a pool (#336).
        *self = self.remount_rw()?;
        Ok(())
    }
}

/// `BTRFS_UUID_TREE_OBJECTID`: the index from a subvolume's UUID to its id.
const UUID_TREE_OBJECTID: u64 = 9;

/// `BTRFS_UUID_KEY_SUBVOL`: a UUID tree item naming the subvolume whose own
/// UUID it is filed under.
const UUID_KEY_SUBVOL: u8 = 251;

/// `BTRFS_INODE_ROOT_ITEM_INIT`: set in the inode a `ROOT_ITEM` embeds, so
/// the kernel knows the item is not one a pre-2.6.30 kernel wrote.
const INODE_ROOT_ITEM_INIT: u64 = 1 << 31;

/// The size of `struct btrfs_root_item`, as every `ROOT_ITEM` the kernel
/// writes today is.
const ROOT_ITEM_SIZE: usize = 439;

/// Offsets within `struct btrfs_root_item` past the ones [`root_item`]
/// names, which only a new item needs.
mod root_item_more {
    /// `u64`. The bytes of tree blocks the tree holds.
    pub const BYTES_USED: usize = 192;
    /// `u32`. How many directory entries refer to the tree.
    pub const REFS: usize = 216;
    /// `btrfs_disk_key`. Where the cleaner's drop of a dead tree got to.
    pub const DROP_PROGRESS: usize = 220;
    /// `u8`. The level the cleaner's drop got to.
    pub const DROP_LEVEL: usize = 237;
    /// `[u8; 16]`. The subvolume's own UUID.
    pub const UUID: usize = 247;
    /// `u64`. The transaction that last changed the tree's contents.
    pub const CTRANSID: usize = 295;
    /// `u64`. The transaction that made the tree.
    pub const OTRANSID: usize = 303;
    /// `btrfs_timespec`. When the tree's contents last changed.
    pub const CTIME: usize = 327;
    /// `btrfs_timespec`. When the tree was made.
    pub const OTIME: usize = 339;
}

/// The top directory of a subvolume, inode 256 in every tree the kernel
/// makes.
const TOP_DIR: u64 = FIRST_FREE_OBJECTID;

impl Filesystem {
    /// Create an empty subvolume `name` in directory `parent` of the
    /// top-level subvolume, and commit it. Returns its id.
    ///
    /// What `btrfs subvolume create` does, as one transaction: a new tree
    /// whose one leaf holds the subvolume's top directory (inode 256, with
    /// permission bits `mode` and owned by `uid`:`gid`), its `ROOT_ITEM`,
    /// `ROOT_REF` and `ROOT_BACKREF` in the root tree, its UUID in the
    /// UUID tree, and an entry in `parent` naming the new tree. Each was
    /// read off subvolumes the kernel made:
    ///
    /// - the id is one past the highest objectid in the root tree's free
    ///   range, and never below 256;
    /// - the entry's location is the `ROOT_ITEM`'s key, offset zero, where
    ///   a snapshot's entry has an offset of all ones;
    /// - the `ROOT_ITEM` embeds a directory inode of generation 1, size 3,
    ///   one link, a node's worth of bytes, mode 040755 and flags
    ///   `ROOT_ITEM_INIT`, and carries the transaction as its generation,
    ///   `ctransid` and `otransid`;
    /// - the top directory starts with sequence zero and no inode flags: a
    ///   subvolume does not inherit the flags of the directory it is made
    ///   in, as an ordinary directory does.
    ///
    /// A subvolume does inherit the `btrfs.*` properties of its parent
    /// subvolume's top directory, which this does not copy, so a top-level
    /// subvolume carrying one is refused rather than given a subvolume
    /// that silently lacks it.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless mounted with [`Filesystem::mount_rw`],
    /// or when the top-level subvolume is read-only;
    /// [`Error::NotADirectory`] if `parent` is not one;
    /// [`Error::UnsupportedFeature`] for a name that is invalid or already
    /// present, a mount of a subvolume other than the top level, a
    /// filesystem with no UUID tree or with quotas enabled (whose quota
    /// group for the subvolume is not made), a top-level subvolume with a
    /// `btrfs.*` property to inherit, and whatever the planner refuses --
    /// among them a root tree leaf that would have to split while it is
    /// the root tree's only block. Nothing is written unless everything
    /// is.
    pub fn create_subvolume(
        &mut self,
        parent: u64,
        name: &[u8],
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<u64> {
        use crate::chunk::objectid::ROOT_TREE;
        use crate::dir::{ftype, name_hash, offsets as d, DIR_INDEX_KEY, DIR_ITEM_KEY};
        use crate::inode::{offsets as io, INODE_ITEM_KEY, INODE_ITEM_SIZE, INODE_REF_KEY};
        use crate::leaf_edit::OwnedItem;
        use crate::namespace::{now, touch};
        use crate::transaction::{ItemEdit, NewTree};

        self.require_writable_top_level()?;
        let items = self.root_tree_items()?;
        let top_flags = items
            .iter()
            .find(|(o, t, _, d)| {
                *o == FS_TREE_OBJECTID && *t == ROOT_ITEM_KEY && d.len() >= root_item::MIN_SIZE
            })
            .map(|(_, _, _, d)| le64(d, root_item::FLAGS))
            .ok_or(Error::NotFound)?;
        if top_flags & ROOT_SUBVOL_RDONLY != 0 {
            return Err(Error::ReadOnly);
        }
        if self.tree_root(UUID_TREE_OBJECTID).is_err() {
            return Err(Error::UnsupportedFeature(
                "the filesystem has no UUID tree to index a new subvolume in".into(),
            ));
        }
        if self.tree_root(QUOTA_TREE_OBJECTID).is_ok() {
            return Err(Error::UnsupportedFeature(
                "quotas are enabled, and the quota group a new subvolume belongs to is not \
                 created"
                    .into(),
            ));
        }
        let top = self.root_inode()?.ino;
        if let Some(prop) = self
            .list_xattrs(top)?
            .into_iter()
            .find(|x| x.name.starts_with(b"btrfs."))
        {
            return Err(Error::UnsupportedFeature(format!(
                "the top-level subvolume carries the property {:?}, which a new subvolume \
                 inherits, and inheriting it is not implemented",
                String::from_utf8_lossy(&prop.name)
            )));
        }
        let dir = self.writable_parent(parent, name)?;
        let id = next_subvolume_id(&items)?;
        let generation = self.next_generation()?;
        let now = now();
        let index = self.next_dir_index(parent)?;
        let uuid = crate::mkfs::random_uuid();

        // The parent's two entries, naming the new tree's ROOT_ITEM.
        let mut entry = vec![0u8; d::NAME];
        entry[d::LOCATION..d::LOCATION + 8].copy_from_slice(&id.to_le_bytes());
        entry[d::LOCATION + 8] = ROOT_ITEM_KEY;
        entry[d::TRANSID..d::TRANSID + 8].copy_from_slice(&generation.to_le_bytes());
        entry[d::NAME_LEN..d::NAME_LEN + 2].copy_from_slice(&(name.len() as u16).to_le_bytes());
        entry[d::TYPE] = ftype::DIR;
        entry.extend_from_slice(name);
        let hash_key = key(parent, DIR_ITEM_KEY, name_hash(name));
        let mut by_hash = self.item_bytes(hash_key)?.unwrap_or_default();
        by_hash.extend_from_slice(&entry);
        if by_hash.len() > self.max_item_size() {
            return Err(Error::UnsupportedFeature(format!(
                "directory {parent}'s names sharing the hash of {:?} would outgrow a leaf",
                String::from_utf8_lossy(name)
            )));
        }
        // The directory's size counts the name once per entry.
        let mut parent_raw = self.raw_inode(parent)?;
        let size = le64(&parent_raw, io::SIZE) + 2 * name.len() as u64;
        parent_raw[io::SIZE..io::SIZE + 8].copy_from_slice(&size.to_le_bytes());
        touch(&mut parent_raw, generation, now, &[io::CTIME, io::MTIME]);
        let edits = vec![
            ItemEdit::Put(OwnedItem {
                key: hash_key,
                data: by_hash,
            }),
            ItemEdit::Put(OwnedItem {
                key: key(parent, DIR_INDEX_KEY, index),
                data: entry,
            }),
            ItemEdit::Put(OwnedItem {
                key: key(parent, INODE_ITEM_KEY, 0),
                data: parent_raw,
            }),
        ];

        // The root tree's three items, and the UUID tree's one.
        let mut reference = Vec::with_capacity(root_ref::NAME + name.len());
        reference.extend_from_slice(&parent.to_le_bytes());
        reference.extend_from_slice(&index.to_le_bytes());
        reference.extend_from_slice(&(name.len() as u16).to_le_bytes());
        reference.extend_from_slice(name);
        let uuid_key = key(
            u64::from_le_bytes(uuid[..8].try_into().expect("8 bytes")),
            UUID_KEY_SUBVOL,
            u64::from_le_bytes(uuid[8..].try_into().expect("8 bytes")),
        );
        // A UUID already filed holds a list of ids, which this one joins.
        let mut ids = {
            let reader = self.pool_reader();
            reader
                .tree()
                .search(self.tree_root(UUID_TREE_OBJECTID)?, &uuid_key)?
                .map(|item| item.data)
                .unwrap_or_default()
        };
        ids.extend_from_slice(&id.to_le_bytes());
        let tree_edits = vec![
            (
                ROOT_TREE,
                ItemEdit::Put(OwnedItem {
                    key: key(FS_TREE_OBJECTID, ROOT_REF_KEY, id),
                    data: reference.clone(),
                }),
            ),
            (
                ROOT_TREE,
                ItemEdit::Put(OwnedItem {
                    key: key(id, ROOT_ITEM_KEY, 0),
                    data: new_root_item(self.sb.nodesize, generation, uuid, now),
                }),
            ),
            (
                ROOT_TREE,
                ItemEdit::Put(OwnedItem {
                    key: key(id, ROOT_BACKREF_KEY, FS_TREE_OBJECTID),
                    data: reference,
                }),
            ),
            (
                UUID_TREE_OBJECTID,
                ItemEdit::Put(OwnedItem {
                    key: uuid_key,
                    data: ids,
                }),
            ),
        ];

        // The new tree: its top directory, flags left clear, and that
        // directory's `..`.
        let mut inode = vec![0u8; INODE_ITEM_SIZE];
        inode[io::GENERATION..io::GENERATION + 8].copy_from_slice(&generation.to_le_bytes());
        inode[io::TRANSID..io::TRANSID + 8].copy_from_slice(&generation.to_le_bytes());
        inode[io::NLINK..io::NLINK + 4].copy_from_slice(&1u32.to_le_bytes());
        inode[io::UID..io::UID + 4].copy_from_slice(&uid.to_le_bytes());
        inode[io::GID..io::GID + 4].copy_from_slice(&gid.to_le_bytes());
        inode[io::MODE..io::MODE + 4].copy_from_slice(&(S_IFDIR | (mode & 0o7777)).to_le_bytes());
        for at in [io::ATIME, io::CTIME, io::MTIME, io::OTIME] {
            put_time(&mut inode, at, now);
        }
        let mut dotdot = 0u64.to_le_bytes().to_vec();
        dotdot.extend_from_slice(&2u16.to_le_bytes());
        dotdot.extend_from_slice(b"..");
        let new_trees = vec![NewTree {
            objectid: id,
            items: vec![
                OwnedItem {
                    key: key(TOP_DIR, INODE_ITEM_KEY, 0),
                    data: inode,
                },
                OwnedItem {
                    key: key(TOP_DIR, INODE_REF_KEY, TOP_DIR),
                    data: dotdot,
                },
            ],
        }];

        self.commit_write(
            DataWrite {
                root: FS_TREE_OBJECTID,
                ino: dir.ino,
                time: now,
                edits,
                tree_edits,
                new_trees,
                ..Default::default()
            },
            generation,
        )?;
        Ok(id)
    }

    /// Delete the subvolume `name` in directory `parent` of the top-level
    /// subvolume, and commit it.
    ///
    /// What `btrfs subvolume delete` does, lazily and as one transaction:
    /// the parent loses both its entries for the name, and its size, times
    /// and change counter move as for any unlink; the root tree loses the
    /// `ROOT_REF` and `ROOT_BACKREF`; the UUID tree loses the subvolume's
    /// UUID (and its received UUID, if it has one); and the `ROOT_ITEM` is
    /// marked dead -- no references, `BTRFS_ROOT_SUBVOL_DEAD` set, drop
    /// progress cleared -- with an `ORPHAN_ITEM` filed under its id in the
    /// root tree. The subvolume's own tree is left as it is, for the
    /// kernel's cleaner to drop on the next read-write mount, as it drops
    /// one the kernel deleted. A read-only subvolume is deleted like any
    /// other, as the kernel deletes a read-only snapshot.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless mounted with [`Filesystem::mount_rw`],
    /// or when the top-level subvolume is read-only;
    /// [`Error::NotADirectory`] if `parent` is not one;
    /// [`Error::NotFound`] for a name that is absent or names anything but
    /// a subvolume; [`Error::UnsupportedFeature`] for the default
    /// subvolume (which the kernel refuses with `EPERM`), one that holds
    /// another subvolume (`ENOTEMPTY`), a mount of a subvolume other than
    /// the top level, a volume with quotas enabled, and whatever the
    /// planner refuses. Nothing is written unless everything is.
    pub fn delete_subvolume(&mut self, parent: u64, name: &[u8]) -> Result<()> {
        use crate::chunk::objectid::ROOT_TREE;
        use crate::dir::{name_hash, DIR_INDEX_KEY, DIR_ITEM_KEY};
        use crate::inode::{offsets as io, INODE_ITEM_KEY};
        use crate::leaf_edit::OwnedItem;
        use crate::namespace::{now, touch, without_dir_record};
        use crate::transaction::ItemEdit;

        self.require_writable_top_level()?;
        let items = self.root_tree_items()?;
        let top_flags = items
            .iter()
            .find(|(o, t, _, d)| {
                *o == FS_TREE_OBJECTID && *t == ROOT_ITEM_KEY && d.len() >= root_item::MIN_SIZE
            })
            .map(|(_, _, _, d)| le64(d, root_item::FLAGS))
            .ok_or(Error::NotFound)?;
        if top_flags & ROOT_SUBVOL_RDONLY != 0 {
            return Err(Error::ReadOnly);
        }
        let entry = self.lookup_entry(parent, name)?;
        if entry.location_type != ROOT_ITEM_KEY {
            return Err(Error::NotFound);
        }
        let id = entry.ino;
        let (root_key_offset, mut root) = items
            .iter()
            .find(|(o, t, _, _)| *o == id && *t == ROOT_ITEM_KEY)
            .map(|(_, _, off, d)| (*off, d.clone()))
            .ok_or(Error::NotFound)?;
        if root.len() < root_item_more::DROP_LEVEL + 1 {
            return Err(Error::UnsupportedFeature(format!(
                "subvolume {id}'s root item is {} bytes, too short to mark dead",
                root.len()
            )));
        }
        if self.default_subvolume()? == id {
            return Err(Error::UnsupportedFeature(format!(
                "subvolume {id} is the default subvolume, which is not deleted"
            )));
        }
        if items
            .iter()
            .any(|(o, t, _, _)| *o == id && *t == ROOT_REF_KEY)
        {
            return Err(Error::UnsupportedFeature(format!(
                "subvolume {id} holds other subvolumes, which go first"
            )));
        }
        if self.tree_root(QUOTA_TREE_OBJECTID).is_ok() {
            return Err(Error::UnsupportedFeature(
                "quotas are enabled, and the quota group a deleted subvolume belongs to is \
                 not accounted"
                    .into(),
            ));
        }
        let reference = items
            .iter()
            .find(|(o, t, off, _)| *o == FS_TREE_OBJECTID && *t == ROOT_REF_KEY && *off == id)
            .map(|(_, _, _, d)| d.clone())
            .ok_or_else(|| {
                Error::UnsupportedFeature(format!(
                    "subvolume {id} has no reference from the top-level subvolume"
                ))
            })?;
        if reference.len() < root_ref::NAME || le64(&reference, root_ref::DIRID) != parent {
            return Err(Error::UnsupportedFeature(format!(
                "subvolume {id}'s reference does not name directory {parent}"
            )));
        }
        let index = le64(&reference, root_ref::SEQUENCE);
        let generation = self.next_generation()?;
        let now = now();

        // The parent's two entries, and its size, which counts the name
        // once per entry.
        let hash_key = key(parent, DIR_ITEM_KEY, name_hash(name));
        let by_hash = self.item_bytes(hash_key)?.ok_or(Error::NotFound)?;
        let rest = without_dir_record(&by_hash, name).ok_or(Error::NotFound)?;
        let index_key = key(parent, DIR_INDEX_KEY, index);
        if self.item_bytes(index_key)?.is_none() {
            return Err(Error::UnsupportedFeature(format!(
                "directory {parent} has no index entry {index} for subvolume {id}"
            )));
        }
        let mut parent_raw = self.raw_inode(parent)?;
        let size = le64(&parent_raw, io::SIZE)
            .checked_sub(2 * name.len() as u64)
            .ok_or_else(|| {
                Error::UnsupportedFeature(format!(
                    "directory {parent}'s size is smaller than its entries for subvolume {id}"
                ))
            })?;
        parent_raw[io::SIZE..io::SIZE + 8].copy_from_slice(&size.to_le_bytes());
        touch(&mut parent_raw, generation, now, &[io::CTIME, io::MTIME]);
        let edits = vec![
            if rest.is_empty() {
                ItemEdit::Delete(hash_key)
            } else {
                ItemEdit::Put(OwnedItem {
                    key: hash_key,
                    data: rest,
                })
            },
            ItemEdit::Delete(index_key),
            ItemEdit::Put(OwnedItem {
                key: key(parent, INODE_ITEM_KEY, 0),
                data: parent_raw,
            }),
        ];

        // The root item, dead: no references, the dead flag, and a drop
        // that starts from the beginning, as `btrfs_delete_subvolume`
        // leaves it.
        let flags = le64(&root, root_item::FLAGS) | ROOT_SUBVOL_DEAD;
        root[root_item::FLAGS..root_item::FLAGS + 8].copy_from_slice(&flags.to_le_bytes());
        root[root_item_more::REFS..root_item_more::REFS + 4].fill(0);
        root[root_item_more::DROP_PROGRESS..=root_item_more::DROP_LEVEL].fill(0);
        let mut tree_edits = vec![
            (
                ROOT_TREE,
                ItemEdit::Delete(key(FS_TREE_OBJECTID, ROOT_REF_KEY, id)),
            ),
            (
                ROOT_TREE,
                ItemEdit::Put(OwnedItem {
                    key: key(id, ROOT_ITEM_KEY, root_key_offset),
                    data: root.clone(),
                }),
            ),
            (
                ROOT_TREE,
                ItemEdit::Delete(key(id, ROOT_BACKREF_KEY, FS_TREE_OBJECTID)),
            ),
            (
                ROOT_TREE,
                ItemEdit::Put(OwnedItem {
                    key: key(ORPHAN_OBJECTID, ORPHAN_ITEM_KEY, id),
                    data: Vec::new(),
                }),
            ),
        ];

        // The UUID tree's entries for the subvolume, each a list of ids
        // that loses this one. One the kernel finds missing it passes
        // over, and so does this.
        if let Ok(uuid_root) = self.tree_root(UUID_TREE_OBJECTID) {
            for (at, key_type) in [
                (root_item_more::UUID, UUID_KEY_SUBVOL),
                (ROOT_ITEM_RECEIVED_UUID, UUID_KEY_RECEIVED_SUBVOL),
            ] {
                let Some(uuid) = root.get(at..at + 16).filter(|u| u.iter().any(|&b| b != 0)) else {
                    continue;
                };
                let uuid_key = key(le64(uuid, 0), key_type, le64(uuid, 8));
                let held = {
                    let reader = self.pool_reader();
                    reader
                        .tree()
                        .search(uuid_root, &uuid_key)?
                        .map(|item| item.data)
                };
                let Some(held) = held else { continue };
                let ids: Vec<u8> = held
                    .chunks_exact(8)
                    .filter(|c| *c != id.to_le_bytes())
                    .flatten()
                    .copied()
                    .collect();
                if ids.len() == held.len() {
                    continue;
                }
                tree_edits.push((
                    UUID_TREE_OBJECTID,
                    if ids.is_empty() {
                        ItemEdit::Delete(uuid_key)
                    } else {
                        ItemEdit::Put(OwnedItem {
                            key: uuid_key,
                            data: ids,
                        })
                    },
                ));
            }
        }

        self.commit_write(
            DataWrite {
                root: FS_TREE_OBJECTID,
                ino: parent,
                time: now,
                edits,
                tree_edits,
                ..Default::default()
            },
            generation,
        )
    }
}

/// `BTRFS_ROOT_SUBVOL_DEAD`: set in a deleted subvolume's `ROOT_ITEM`
/// flags until the cleaner has dropped its tree.
const ROOT_SUBVOL_DEAD: u64 = 1 << 48;

/// `BTRFS_ORPHAN_OBJECTID`: the objectid orphan items are filed under.
const ORPHAN_OBJECTID: u64 = -5i64 as u64;

/// `BTRFS_ORPHAN_ITEM_KEY`: in the root tree, a tree the cleaner drops.
const ORPHAN_ITEM_KEY: u8 = 48;

/// `BTRFS_UUID_KEY_RECEIVED_SUBVOL`: a UUID tree item naming the
/// subvolumes received from the one whose UUID it is filed under.
const UUID_KEY_RECEIVED_SUBVOL: u8 = 252;

/// `S_IFDIR`.
const S_IFDIR: u32 = 0o040_000;

/// `BTRFS_QUOTA_TREE_OBJECTID`: present while quotas are enabled.
pub(crate) const QUOTA_TREE_OBJECTID: u64 = 8;

/// The id a new subvolume gets: one past the highest objectid in the root
/// tree's free range, as `btrfs_find_highest_objectid` finds it, and never
/// below [`FIRST_FREE_OBJECTID`].
fn next_subvolume_id(items: &[(u64, u8, u64, Vec<u8>)]) -> Result<u64> {
    let highest = items
        .iter()
        .map(|(objectid, ..)| *objectid)
        .filter(|&o| o <= LAST_FREE_OBJECTID)
        .max()
        .unwrap_or(0)
        .max(FIRST_FREE_OBJECTID - 1);
    if highest >= LAST_FREE_OBJECTID {
        return Err(Error::UnsupportedFeature(
            "the root tree has no subvolume ids left".into(),
        ));
    }
    Ok(highest + 1)
}

/// A new subvolume's `ROOT_ITEM`, as the kernel writes one for `btrfs
/// subvolume create`. Its root block is left zero: the renderer points it
/// at wherever the plan puts the tree's leaf.
fn new_root_item(nodesize: u32, generation: u64, uuid: [u8; 16], now: (u64, u32)) -> Vec<u8> {
    use crate::inode::offsets as io;
    use root_item_more as m;
    let nodesize = u64::from(nodesize);
    let mut b = vec![0u8; ROOT_ITEM_SIZE];
    // The embedded inode: a directory nobody reads, filled in as the
    // kernel fills it.
    b[io::GENERATION..io::GENERATION + 8].copy_from_slice(&1u64.to_le_bytes());
    b[io::SIZE..io::SIZE + 8].copy_from_slice(&3u64.to_le_bytes());
    b[io::NBYTES..io::NBYTES + 8].copy_from_slice(&nodesize.to_le_bytes());
    b[io::NLINK..io::NLINK + 4].copy_from_slice(&1u32.to_le_bytes());
    b[io::MODE..io::MODE + 4].copy_from_slice(&(S_IFDIR | 0o755).to_le_bytes());
    b[io::FLAGS..io::FLAGS + 8].copy_from_slice(&INODE_ROOT_ITEM_INIT.to_le_bytes());

    for at in [
        root_item::GENERATION,
        root_item::GENERATION_V2,
        m::CTRANSID,
        m::OTRANSID,
    ] {
        b[at..at + 8].copy_from_slice(&generation.to_le_bytes());
    }
    b[root_item::ROOT_DIRID..root_item::ROOT_DIRID + 8].copy_from_slice(&TOP_DIR.to_le_bytes());
    b[m::BYTES_USED..m::BYTES_USED + 8].copy_from_slice(&nodesize.to_le_bytes());
    b[m::REFS..m::REFS + 4].copy_from_slice(&1u32.to_le_bytes());
    b[m::UUID..m::UUID + 16].copy_from_slice(&uuid);
    put_time(&mut b, m::CTIME, now);
    put_time(&mut b, m::OTIME, now);
    b
}

fn key(objectid: u64, key_type: u8, offset: u64) -> crate::chunk::DiskKey {
    crate::chunk::DiskKey {
        objectid,
        key_type,
        offset,
    }
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

/// A `btrfs_timespec`: seconds, then nanoseconds.
fn put_time(b: &mut [u8], at: usize, t: (u64, u32)) {
    b[at..at + 8].copy_from_slice(&t.0.to_le_bytes());
    b[at + 8..at + 12].copy_from_slice(&t.1.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The id range has two ends, and the upper one is the point.
    #[test]
    fn internal_trees_are_not_subvolumes() {
        assert!(is_subvolume_id(FS_TREE_OBJECTID), "the default subvolume");
        assert!(is_subvolume_id(256), "the first created one");
        assert!(is_subvolume_id(1000));

        assert!(!is_subvolume_id(1), "the root tree");
        assert!(!is_subvolume_id(2), "the extent tree");
        assert!(!is_subvolume_id(255), "below the free range");

        // −9 as unsigned, which is a real tree on a real filesystem and
        // is what a lower bound alone lets through.
        assert!(
            !is_subvolume_id(u64::MAX - 8),
            "a negatively numbered internal tree is not a subvolume"
        );
        assert!(!is_subvolume_id(u64::MAX));
    }

    /// The name sits after a `dirid` and a `sequence`, and its length is
    /// declared rather than implied by the item's size.
    #[test]
    fn a_reference_name_is_read_from_its_declared_length() {
        let mut item = vec![0u8; 18];
        item[16..18].copy_from_slice(&3u16.to_le_bytes());
        item.extend_from_slice(b"sub");
        assert_eq!(reference_name(&item).as_deref(), Some(&b"sub"[..]));

        // A length that runs past the item is refused rather than
        // panicking or reading whatever follows.
        let mut lying = vec![0u8; 18];
        lying[16..18].copy_from_slice(&99u16.to_le_bytes());
        lying.extend_from_slice(b"sub");
        assert_eq!(reference_name(&lying), None);

        assert_eq!(
            reference_name(&[0u8; 4]),
            None,
            "too short to hold a header"
        );
    }

    /// A nested subvolume's path is its whole chain, and the default
    /// one's is the root.
    #[test]
    fn a_path_is_the_chain_of_parents() {
        let mut names = std::collections::BTreeMap::new();
        names.insert(256u64, (5u64, b"sub".to_vec()));
        names.insert(257u64, (256u64, b"inner".to_vec()));

        assert_eq!(path_of(5, &names), "/");
        assert_eq!(path_of(256, &names), "sub");
        assert_eq!(path_of(257, &names), "sub/inner");
    }

    /// A link that carries the directory its entry sits in keeps it, at
    /// its place in the chain (#362).
    #[test]
    fn a_link_through_a_directory_stays_in_the_path() {
        let mut links = std::collections::BTreeMap::new();
        links.insert(256u64, (5u64, b"top/nested".to_vec()));
        links.insert(257u64, (256u64, b"dir/deeper/inner".to_vec()));

        assert_eq!(path_of(256, &links), "top/nested");
        assert_eq!(path_of(257, &links), "top/nested/dir/deeper/inner");
    }

    /// A directory's name is read from its `INODE_REF` by the declared
    /// length, and a length that runs past the item is refused.
    #[test]
    fn an_inode_ref_name_is_read_from_its_declared_length() {
        let mut item = 2u64.to_le_bytes().to_vec();
        item.extend_from_slice(&3u16.to_le_bytes());
        item.extend_from_slice(b"top");
        assert_eq!(inode_ref_name(&item).as_deref(), Some(&b"top"[..]));

        let mut lying = 2u64.to_le_bytes().to_vec();
        lying.extend_from_slice(&99u16.to_le_bytes());
        lying.extend_from_slice(b"top");
        assert_eq!(inode_ref_name(&lying), None);
        assert_eq!(inode_ref_name(&[0u8; 9]), None, "too short for a header");
    }

    /// References that form a cycle must not hang the listing — which is
    /// the tool someone reaches for when they already suspect the
    /// filesystem is damaged.
    #[test]
    fn a_cycle_in_the_references_terminates() {
        let mut names = std::collections::BTreeMap::new();
        names.insert(256u64, (257u64, b"a".to_vec()));
        names.insert(257u64, (256u64, b"b".to_vec()));

        // Any answer will do; not returning is the failure.
        let path = path_of(256, &names);
        assert!(!path.is_empty());
    }

    /// A new subvolume takes the id after the highest in the free range,
    /// whatever internal trees are numbered below and above it.
    #[test]
    fn a_new_subvolume_takes_the_next_free_id() {
        let item = |objectid: u64| (objectid, ROOT_ITEM_KEY, 0u64, Vec::new());
        let negative = u64::MAX - 8;
        assert_eq!(
            next_subvolume_id(&[item(1), item(2), item(5), item(negative)]),
            Ok(FIRST_FREE_OBJECTID)
        );
        assert_eq!(
            next_subvolume_id(&[item(5), item(256), item(259), item(negative)]),
            Ok(260)
        );
        assert!(next_subvolume_id(&[item(LAST_FREE_OBJECTID)]).is_err());
    }

    /// A new subvolume's root item carries its tree's generation, its
    /// top directory and its UUID where the root item's reader finds them.
    #[test]
    fn a_new_root_item_is_filled_where_it_is_read() {
        use root_item_more as m;
        let b = new_root_item(16384, 9, [7; 16], (100, 5));
        assert_eq!(b.len(), ROOT_ITEM_SIZE);
        assert!(b.len() >= root_item::MIN_SIZE);
        for at in [
            root_item::GENERATION,
            root_item::GENERATION_V2,
            m::CTRANSID,
            m::OTRANSID,
        ] {
            assert_eq!(le64(&b, at), 9, "generation at {at}");
        }
        assert_eq!(le64(&b, root_item::ROOT_DIRID), TOP_DIR);
        assert_eq!(le64(&b, root_item::FLAGS), 0, "writable");
        assert_eq!(le64(&b, root_item::LAST_SNAPSHOT), 0);
        assert_eq!(le64(&b, m::BYTES_USED), 16384, "one leaf");
        assert_eq!(&b[m::REFS..m::REFS + 4], &1u32.to_le_bytes());
        assert_eq!(&b[m::UUID..m::UUID + 16], &[7; 16]);
        assert_eq!(le64(&b, m::OTIME), 100);
        assert_eq!(le64(&b, m::CTIME), 100);
        // The root block is the plan's to fill in.
        assert_eq!(le64(&b, root_item::BYTENR), 0);
    }
}
