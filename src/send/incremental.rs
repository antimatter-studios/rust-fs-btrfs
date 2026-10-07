//! Incremental send streams: what changed between two read-only snapshots,
//! as `btrfs send -p PARENT` writes it (#273).
//!
//! The receiver already holds the parent. The stream opens with a
//! `SNAPSHOT` command naming it by UUID and change transaction; the
//! receiver snapshots its copy and the rest of the stream turns that copy
//! into the child.
//!
//! # Matching inodes
//!
//! Both snapshots come from one subvolume, so an inode keeps its number in
//! both. A number paired with a different generation is a different file
//! that reused the number, and is treated as one inode removed and another
//! made. The top directory is the same in both whatever its generation.
//!
//! # Changing the namespace
//!
//! Three passes, so that no rename can land on a name still in use or
//! inside itself, whatever the shapes of the two trees:
//!
//! 1. Every name the parent has and the child does not -- including a name
//!    the child gives to a different inode -- is taken away. A directory,
//!    and the last name of a file the child keeps, is renamed to an orphan
//!    name in the top directory (`o<ino>-<gen>-<n>`, the kernel's own
//!    spelling); any other name is unlinked.
//! 2. The child's names are made top-down, so each directory is in place
//!    before anything goes into it: an orphan is renamed to its new name, a
//!    file that already has a name gets another as a hard link, and an
//!    inode the parent did not have is created as in a full stream.
//! 3. The directories the child does not have, all orphans by now and all
//!    empty, are removed.
//!
//! # Changing what is inside
//!
//! A kept file's data is compared range by range through its extent items:
//! a range is unchanged where both snapshots read it from the same place
//! on disk, or both read zeros (a hole or a preallocated range), or both
//! hold the same inline item. Every other range inside the child's size is
//! written from the child, zeros included, and a file whose data or size
//! changed is truncated to the child's size. Then extended attributes are
//! set or removed, the owner set (and the mode after it, because a change
//! of owner clears set-user-ID), and the mode set. Last, deepest first,
//! the times of every inode that is new, whose times changed, or that the
//! stream touched -- including each directory a name was added to or
//! taken from, whose modification time the receiver moved.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::{attr, cmd, StreamWriter, SEND_WRITE_CHUNK};
use crate::error::{Error, Result};
use crate::fs::{file_extent, Filesystem, EXTENT_DATA_KEY};
use crate::inode::Inode;
use crate::superblock::le64;

/// An inode as both snapshots can name it: its number and generation.
type Id = (u64, u64);

/// The top directory of the subvolume, whatever its generation.
const TOP: Id = (0, 0);

/// A file extent item's type byte: inline, regular, preallocated.
const EXTENT_INLINE: u8 = 0;
const EXTENT_PREALLOC: u8 = 2;

/// One snapshot's namespace, read whole.
struct Snapshot {
    inodes: BTreeMap<Id, Inode>,
    /// Every name, `(directory, name, inode)`, breadth-first from the top.
    entries: Vec<(Id, Vec<u8>, Id)>,
    /// How far below the top each inode is first met.
    depth: BTreeMap<Id, usize>,
}

impl Snapshot {
    fn read(tree: &Filesystem) -> Result<Self> {
        let top = tree.root_inode()?;
        let top_ino = top.ino;
        let mut snap = Snapshot {
            inodes: BTreeMap::from([(TOP, top)]),
            entries: Vec::new(),
            depth: BTreeMap::from([(TOP, 0)]),
        };
        let mut ids: BTreeMap<u64, Id> = BTreeMap::from([(top_ino, TOP)]);
        let mut queue = VecDeque::from([(TOP, top_ino, 0usize)]);
        while let Some((dir, dir_ino, depth)) = queue.pop_front() {
            for entry in tree.read_dir(dir_ino)? {
                if !entry.is_inode() {
                    continue; // a nested subvolume: in neither stream
                }
                let id = match ids.get(&entry.ino) {
                    Some(id) => *id,
                    None => {
                        let inode = tree.read_inode(entry.ino)?;
                        let id = (inode.ino, inode.generation);
                        if inode.is_dir() {
                            queue.push_back((id, inode.ino, depth + 1));
                        }
                        ids.insert(inode.ino, id);
                        snap.depth.insert(id, depth + 1);
                        snap.inodes.insert(id, inode);
                        id
                    }
                };
                snap.entries.push((dir, entry.name, id));
            }
        }
        Ok(snap)
    }
}

/// The receiver's namespace as the stream so far leaves it.
#[derive(Default)]
struct Namespace {
    names: BTreeMap<(Id, Vec<u8>), Id>,
    refs: BTreeMap<Id, BTreeSet<(Id, Vec<u8>)>>,
}

impl Namespace {
    fn add(&mut self, dir: Id, name: &[u8], id: Id) {
        self.names.insert((dir, name.to_vec()), id);
        self.refs
            .entry(id)
            .or_default()
            .insert((dir, name.to_vec()));
    }

    fn remove(&mut self, dir: Id, name: &[u8]) {
        if let Some(id) = self.names.remove(&(dir, name.to_vec())) {
            if let Some(refs) = self.refs.get_mut(&id) {
                refs.remove(&(dir, name.to_vec()));
            }
        }
    }

    fn has_name(&self, id: Id) -> bool {
        self.refs.get(&id).is_some_and(|r| !r.is_empty())
    }

    /// Where `id` is now, from the top. Any of a file's names will do.
    fn path(&self, id: Id) -> Result<Vec<u8>> {
        let mut parts: Vec<&[u8]> = Vec::new();
        let mut at = id;
        while at != TOP {
            let Some((dir, name)) = self.refs.get(&at).and_then(|r| r.iter().next()) else {
                return Err(Error::BadSuperblock(format!(
                    "inode {} has no name while a send stream is being written",
                    at.0
                )));
            };
            parts.push(name);
            at = *dir;
            if parts.len() > self.refs.len() {
                return Err(Error::BadSuperblock(format!(
                    "inode {} is inside itself while a send stream is being written",
                    id.0
                )));
            }
        }
        parts.reverse();
        Ok(parts.join(&b'/'))
    }

    fn entry_path(&self, dir: Id, name: &[u8]) -> Result<Vec<u8>> {
        let mut path = self.path(dir)?;
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(name);
        Ok(path)
    }
}

/// What one range of a file reads from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Backing {
    /// Zeros: a hole, a preallocated range, or past the end of the file.
    Zero,
    /// Bytes on disk: the extent's address, its compression, and where
    /// file offset 0 would fall in its decoded bytes (wrapping).
    Disk {
        bytenr: u64,
        compression: u8,
        shift: u64,
    },
    /// An inline item, compared whole.
    Inline(Vec<u8>),
}

/// A file's extent items as `(start, end, backing)`, in order.
fn segments(tree: &Filesystem, ino: u64) -> Result<Vec<(u64, u64, Backing)>> {
    let mut out = Vec::new();
    for ((objectid, key_type, start), data) in tree.item_run(ino, EXTENT_DATA_KEY)? {
        if objectid != ino || key_type != EXTENT_DATA_KEY {
            break;
        }
        let item_len = data.len();
        let short = |need: usize| {
            Error::BadSuperblock(format!(
                "inode {ino}: extent item at {start} is {item_len} bytes, {need} needed"
            ))
        };
        if data.len() <= file_extent::TYPE {
            return Err(short(file_extent::TYPE + 1));
        }
        let kind = data[file_extent::TYPE];
        if kind == EXTENT_INLINE {
            let len = le64(&data, file_extent::RAM_BYTES);
            out.push((start, start.saturating_add(len), Backing::Inline(data)));
            continue;
        }
        if data.len() < file_extent::REGULAR_SIZE {
            return Err(short(file_extent::REGULAR_SIZE));
        }
        let bytenr = le64(&data, file_extent::DISK_BYTENR);
        let len = le64(&data, file_extent::NUM_BYTES);
        let backing = if bytenr == 0 || kind == EXTENT_PREALLOC {
            Backing::Zero
        } else {
            Backing::Disk {
                bytenr,
                compression: data[file_extent::COMPRESSION],
                shift: le64(&data, file_extent::OFFSET).wrapping_sub(start),
            }
        };
        out.push((start, start.saturating_add(len), backing));
    }
    Ok(out)
}

/// What `segments` says is at `at`, reading zeros past `size`.
fn backing_at(segments: &[(u64, u64, Backing)], at: u64, size: u64) -> &Backing {
    if at >= size {
        return &Backing::Zero;
    }
    segments
        .iter()
        .find(|(s, e, _)| *s <= at && at < *e)
        .map_or(&Backing::Zero, |(_, _, b)| b)
}

/// The ranges of the child's file that do not read the same as the
/// parent's, inside the child's size, merged.
fn changed_ranges(
    old: &[(u64, u64, Backing)],
    old_size: u64,
    new: &[(u64, u64, Backing)],
    new_size: u64,
) -> Vec<(u64, u64)> {
    let mut cuts: BTreeSet<u64> = BTreeSet::from([0, new_size, old_size.min(new_size)]);
    for (s, e, _) in old.iter().chain(new) {
        cuts.insert((*s).min(new_size));
        cuts.insert((*e).min(new_size));
    }
    let cuts: Vec<u64> = cuts.into_iter().collect();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for pair in cuts.windows(2) {
        let (s, e) = (pair[0], pair[1]);
        if s >= e || backing_at(old, s, old_size) == backing_at(new, s, new_size) {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.1 == s => last.1 = e,
            _ => out.push((s, e)),
        }
    }
    out
}

impl Filesystem {
    /// An incremental version-1 send stream of subvolume `id` against
    /// `parent`, as `btrfs send -p` writes one: what a receiver holding
    /// `parent` needs to rebuild `id`.
    ///
    /// Both must be read-only, as the kernel requires. Subvolumes nested
    /// inside either are left out, as in a full stream. See the [module
    /// documentation](self) for the order of the commands.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when either id names no subvolume,
    /// [`Error::UnsupportedFeature`] for one that is not read-only, and
    /// whatever reading the trees returns.
    pub fn send_subvolume_incremental(&self, id: u64, parent: u64) -> Result<Vec<u8>> {
        let child_id = self.send_identity(id)?;
        let parent_id = self.send_identity(parent)?;
        let ctree = self.open_subvolume_at(child_id.bytenr)?;
        let ptree = self.open_subvolume_at(parent_id.bytenr)?;
        let p = Snapshot::read(&ptree)?;
        let c = Snapshot::read(&ctree)?;

        let mut w = StreamWriter::new();
        w.begin(cmd::SNAPSHOT);
        w.attr(attr::PATH, &child_id.name)?;
        w.attr(attr::UUID, &child_id.uuid)?;
        w.attr_u64(attr::CTRANSID, child_id.ctransid)?;
        w.attr(attr::CLONE_UUID, &parent_id.uuid)?;
        w.attr_u64(attr::CLONE_CTRANSID, parent_id.ctransid)?;

        let mut ns = Namespace::default();
        for (dir, name, id) in &p.entries {
            ns.add(*dir, name, *id);
        }
        let wanted: BTreeSet<(Id, &[u8], Id)> = c
            .entries
            .iter()
            .map(|(d, n, i)| (*d, n.as_slice(), *i))
            .collect();
        let top_names: BTreeSet<&[u8]> = c
            .entries
            .iter()
            .filter(|(d, _, _)| *d == TOP)
            .map(|(_, n, _)| n.as_slice())
            .collect();
        let mut touched: BTreeSet<Id> = BTreeSet::new();
        let mut orphans: BTreeMap<Id, Vec<u8>> = BTreeMap::new();

        // 1. Take away every name the child does not have.
        for (dir, name, id) in &p.entries {
            if wanted.contains(&(*dir, name.as_slice(), *id)) {
                continue;
            }
            let path = ns.entry_path(*dir, name)?;
            let kept = c.inodes.contains_key(id);
            let last_name = ns.refs.get(id).map_or(0, BTreeSet::len) <= 1;
            ns.remove(*dir, name);
            if p.inodes[id].is_dir() || (kept && last_name) {
                let orphan = orphan_name(&ns, &top_names, *id);
                w.begin(cmd::RENAME);
                w.attr(attr::PATH, &path)?;
                w.attr(attr::PATH_TO, &orphan)?;
                ns.add(TOP, &orphan, *id);
                orphans.insert(*id, orphan);
                touched.insert(TOP);
            } else {
                w.begin(cmd::UNLINK);
                w.attr(attr::PATH, &path)?;
            }
            touched.insert(*dir);
            touched.insert(*id);
        }

        // 2. Make the child's names, top-down.
        let mut created: BTreeSet<Id> = BTreeSet::new();
        for (dir, name, id) in &c.entries {
            if ns.names.get(&(*dir, name.clone())) == Some(id) {
                continue;
            }
            let path = ns.entry_path(*dir, name)?;
            if let Some(orphan) = orphans.remove(id) {
                w.begin(cmd::RENAME);
                w.attr(attr::PATH, &orphan)?;
                w.attr(attr::PATH_TO, &path)?;
                ns.remove(TOP, &orphan);
            } else if ns.has_name(*id) {
                let existing = ns.path(*id)?;
                w.begin(cmd::LINK);
                w.attr(attr::PATH, &path)?;
                w.attr(attr::PATH_LINK, &existing)?;
            } else {
                self.send_create(&ctree, &mut w, &path, &c.inodes[id])?;
                created.insert(*id);
            }
            ns.add(*dir, name, *id);
            touched.insert(*dir);
            touched.insert(*id);
        }

        // 3. Remove the directories the child does not have: orphans, and
        //    empty, since every name inside them was taken away in pass 1.
        for (id, orphan) in &orphans {
            if c.inodes.contains_key(id) {
                return Err(Error::BadSuperblock(format!(
                    "inode {} is in both snapshots and was given no name in the child",
                    id.0
                )));
            }
            w.begin(cmd::RMDIR);
            w.attr(attr::PATH, orphan)?;
            ns.remove(TOP, orphan);
        }

        // 4. What changed inside each inode both snapshots have.
        for (id, new) in &c.inodes {
            if created.contains(id) {
                continue;
            }
            let Some(old) = p.inodes.get(id) else {
                continue; // made in pass 2
            };
            let path = ns.path(*id)?;
            if new.is_regular_file()
                && self.send_changed_data(&ptree, &ctree, &mut w, &path, old, new)?
            {
                touched.insert(*id);
            }
            let old_x: BTreeMap<Vec<u8>, Vec<u8>> = ptree
                .list_xattrs(old.ino)?
                .into_iter()
                .map(|x| (x.name, x.value))
                .collect();
            let new_x: BTreeMap<Vec<u8>, Vec<u8>> = ctree
                .list_xattrs(new.ino)?
                .into_iter()
                .map(|x| (x.name, x.value))
                .collect();
            for (name, value) in &new_x {
                if old_x.get(name) != Some(value) {
                    w.begin(cmd::SET_XATTR);
                    w.attr(attr::PATH, &path)?;
                    w.attr(attr::XATTR_NAME, name)?;
                    w.attr(attr::XATTR_DATA, value)?;
                }
            }
            for name in old_x.keys().filter(|n| !new_x.contains_key(*n)) {
                w.begin(cmd::REMOVE_XATTR);
                w.attr(attr::PATH, &path)?;
                w.attr(attr::XATTR_NAME, name)?;
            }
            let chowned = old.uid != new.uid || old.gid != new.gid;
            if chowned {
                w.begin(cmd::CHOWN);
                w.attr(attr::PATH, &path)?;
                w.attr_u64(attr::UID, u64::from(new.uid))?;
                w.attr_u64(attr::GID, u64::from(new.gid))?;
            }
            if !new.is_symlink() && (chowned || old.permissions() != new.permissions()) {
                w.begin(cmd::CHMOD);
                w.attr(attr::PATH, &path)?;
                w.attr_u64(attr::MODE, u64::from(new.permissions()))?;
            }
        }
        for id in &created {
            let inode = &c.inodes[id];
            let path = ns.path(*id)?;
            w.begin(cmd::CHOWN);
            w.attr(attr::PATH, &path)?;
            w.attr_u64(attr::UID, u64::from(inode.uid))?;
            w.attr_u64(attr::GID, u64::from(inode.gid))?;
            if !inode.is_symlink() {
                w.begin(cmd::CHMOD);
                w.attr(attr::PATH, &path)?;
                w.attr_u64(attr::MODE, u64::from(inode.permissions()))?;
            }
        }

        // 5. Times last, deepest first: a change below a directory moves its.
        let mut timed: Vec<Id> = c
            .inodes
            .iter()
            .filter(|(id, new)| {
                created.contains(id)
                    || touched.contains(id)
                    || p.inodes.get(id).is_none_or(|old| {
                        (old.atime, old.mtime, old.ctime) != (new.atime, new.mtime, new.ctime)
                    })
            })
            .map(|(id, _)| *id)
            .collect();
        timed.sort_by_key(|id| std::cmp::Reverse(c.depth[id]));
        for id in timed {
            let inode = &c.inodes[&id];
            w.begin(cmd::UTIMES);
            w.attr(attr::PATH, &ns.path(id)?)?;
            w.attr_time(attr::ATIME, inode.atime)?;
            w.attr_time(attr::MTIME, inode.mtime)?;
            w.attr_time(attr::CTIME, inode.ctime)?;
        }
        Ok(w.finish())
    }

    /// The `WRITE`s and `TRUNCATE` that turn the parent's copy of a file
    /// into the child's. Whether anything was sent.
    fn send_changed_data(
        &self,
        ptree: &Filesystem,
        ctree: &Filesystem,
        w: &mut StreamWriter,
        path: &[u8],
        old: &Inode,
        new: &Inode,
    ) -> Result<bool> {
        let ranges = changed_ranges(
            &segments(ptree, old.ino)?,
            old.size,
            &segments(ctree, new.ino)?,
            new.size,
        );
        for (start, end) in &ranges {
            let mut pos = *start;
            while pos < *end {
                let n = (end - pos).min(SEND_WRITE_CHUNK as u64) as usize;
                let mut buf = vec![0u8; n];
                let got = ctree.read_at(new.ino, pos, &mut buf)?;
                buf.truncate(got);
                if buf.is_empty() {
                    break;
                }
                w.begin(cmd::WRITE);
                w.attr(attr::PATH, path)?;
                w.attr_u64(attr::FILE_OFFSET, pos)?;
                w.attr(attr::DATA, &buf)?;
                pos += buf.len() as u64;
            }
        }
        if ranges.is_empty() && old.size == new.size {
            return Ok(false);
        }
        w.begin(cmd::TRUNCATE);
        w.attr(attr::PATH, path)?;
        w.attr_u64(attr::SIZE, new.size)?;
        Ok(true)
    }
}

/// A name in the top directory for an inode between its old name and its
/// new one: free now, and not one the child uses there.
fn orphan_name(ns: &Namespace, top_names: &BTreeSet<&[u8]>, (ino, generation): Id) -> Vec<u8> {
    (0u64..)
        .map(|n| format!("o{ino}-{generation}-{n}").into_bytes())
        .find(|name| {
            !ns.names.contains_key(&(TOP, name.clone())) && !top_names.contains(name.as_slice())
        })
        .expect("an unbounded range has a free name")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(bytenr: u64, shift: u64) -> Backing {
        Backing::Disk {
            bytenr,
            compression: 0,
            shift,
        }
    }

    /// Overwriting the middle of a one-extent file splits its item in
    /// three; only the middle reads from somewhere new.
    #[test]
    fn a_rewritten_middle_is_the_only_change() {
        let old = vec![(0, 200_704, disk(1 << 20, 0))];
        let new = vec![
            (0, 40_960, disk(1 << 20, 0)),
            (40_960, 53_248, disk(9 << 20, 0u64.wrapping_sub(40_960))),
            (53_248, 200_704, disk(1 << 20, 0)),
        ];
        assert_eq!(
            changed_ranges(&old, 200_000, &new, 200_000),
            vec![(40_960, 53_248)]
        );
    }

    /// A punched hole is a change from data to zeros; a hole that stays a
    /// hole, and a preallocated range against one, is not.
    #[test]
    fn zeros_compare_equal_and_a_punched_hole_does_not() {
        let old = vec![(0, 8192, disk(1 << 20, 0)), (16_384, 24_576, Backing::Zero)];
        let new = vec![(0, 4096, disk(1 << 20, 0))];
        assert_eq!(
            changed_ranges(&old, 24_576, &new, 24_576),
            vec![(4096, 8192)]
        );
    }

    /// Growing and shrinking: what lies past the parent's size reads as
    /// zeros there, and nothing past the child's size is sent.
    #[test]
    fn sizes_bound_the_comparison() {
        let old = vec![(0, 8192, disk(1 << 20, 0))];
        assert_eq!(changed_ranges(&old, 5000, &old, 5000), vec![]);
        // The same item, the file grown into its tail: the parent read
        // zeros there, the child reads the extent.
        assert_eq!(changed_ranges(&old, 5000, &old, 8192), vec![(5000, 8192)]);
        assert_eq!(changed_ranges(&old, 8192, &[], 100), vec![(0, 100)]);
    }

    /// The same inline item is no change; a different one is.
    #[test]
    fn inline_items_compare_whole() {
        let a = vec![(0, 5, Backing::Inline(b"aaaaa".to_vec()))];
        let b = vec![(0, 6, Backing::Inline(b"bbbbbb".to_vec()))];
        assert_eq!(changed_ranges(&a, 5, &a, 5), vec![]);
        assert_eq!(changed_ranges(&a, 5, &b, 6), vec![(0, 6)]);
    }

    /// A path is rebuilt from the names the stream has left so far.
    #[test]
    fn a_path_follows_the_current_names() {
        let mut ns = Namespace::default();
        ns.add(TOP, b"a", (257, 1));
        ns.add((257, 1), b"b", (258, 1));
        assert_eq!(ns.path((258, 1)).unwrap(), b"a/b");
        ns.remove(TOP, b"a");
        ns.add(TOP, b"o257-1-0", (257, 1));
        assert_eq!(ns.path((258, 1)).unwrap(), b"o257-1-0/b");
        assert_eq!(ns.entry_path(TOP, b"c").unwrap(), b"c");
    }
}
