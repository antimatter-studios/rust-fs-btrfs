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

/// One change to the receiver's namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NameOp {
    Rename { from: Vec<u8>, to: Vec<u8> },
    Unlink(Vec<u8>),
    Link { path: Vec<u8>, existing: Vec<u8> },
    Create { path: Vec<u8>, id: Id },
    Rmdir(Vec<u8>),
}

/// The namespace changes that turn the parent's names into the child's.
struct NamePlan {
    ops: Vec<NameOp>,
    /// The receiver's names once they are done: the child's.
    ns: Namespace,
    /// Every inode a change named, and every directory one went into or
    /// out of.
    touched: BTreeSet<Id>,
    /// The inodes made new.
    created: BTreeSet<Id>,
}

/// The three passes of the [module documentation](self), over the two
/// snapshots' names (`(directory, name, inode)`, breadth-first). `was_dir`
/// says whether a parent's inode is a directory; `kept` whether the child
/// has it.
fn plan_names(
    parent: &[(Id, Vec<u8>, Id)],
    child: &[(Id, Vec<u8>, Id)],
    was_dir: impl Fn(Id) -> bool,
    kept: impl Fn(Id) -> bool,
) -> Result<NamePlan> {
    let mut ns = Namespace::default();
    for (dir, name, id) in parent {
        ns.add(*dir, name, *id);
    }
    let wanted: BTreeSet<(Id, &[u8], Id)> = child
        .iter()
        .map(|(d, n, i)| (*d, n.as_slice(), *i))
        .collect();
    let top_names: BTreeSet<&[u8]> = child
        .iter()
        .filter(|(d, _, _)| *d == TOP)
        .map(|(_, n, _)| n.as_slice())
        .collect();
    let mut ops = Vec::new();
    let mut touched: BTreeSet<Id> = BTreeSet::new();
    let mut orphans: BTreeMap<Id, Vec<u8>> = BTreeMap::new();

    // 1. Take away every name the child does not have.
    for (dir, name, id) in parent {
        if wanted.contains(&(*dir, name.as_slice(), *id)) {
            continue;
        }
        let path = ns.entry_path(*dir, name)?;
        let last_name = ns.refs.get(id).map_or(0, BTreeSet::len) <= 1;
        ns.remove(*dir, name);
        if was_dir(*id) || (kept(*id) && last_name) {
            let orphan = orphan_name(&ns, &top_names, *id);
            ops.push(NameOp::Rename {
                from: path,
                to: orphan.clone(),
            });
            ns.add(TOP, &orphan, *id);
            orphans.insert(*id, orphan);
            touched.insert(TOP);
        } else {
            ops.push(NameOp::Unlink(path));
        }
        touched.insert(*dir);
        touched.insert(*id);
    }

    // 2. Make the child's names, top-down.
    let mut created: BTreeSet<Id> = BTreeSet::new();
    for (dir, name, id) in child {
        if ns.names.get(&(*dir, name.clone())) == Some(id) {
            continue;
        }
        let path = ns.entry_path(*dir, name)?;
        if let Some(orphan) = orphans.remove(id) {
            ops.push(NameOp::Rename {
                from: orphan.clone(),
                to: path,
            });
            ns.remove(TOP, &orphan);
        } else if ns.has_name(*id) {
            ops.push(NameOp::Link {
                path,
                existing: ns.path(*id)?,
            });
        } else {
            ops.push(NameOp::Create { path, id: *id });
            created.insert(*id);
        }
        ns.add(*dir, name, *id);
        touched.insert(*dir);
        touched.insert(*id);
    }

    // 3. Remove the directories the child does not have: orphans, and
    //    empty, since every name inside them was taken away in pass 1.
    for (id, orphan) in &orphans {
        if kept(*id) {
            return Err(Error::BadSuperblock(format!(
                "inode {} is in both snapshots and was given no name in the child",
                id.0
            )));
        }
        ops.push(NameOp::Rmdir(orphan.clone()));
        ns.remove(TOP, orphan);
    }
    Ok(NamePlan {
        ops,
        ns,
        touched,
        created,
    })
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

        let plan = plan_names(
            &p.entries,
            &c.entries,
            |id| p.inodes[&id].is_dir(),
            |id| c.inodes.contains_key(&id),
        )?;
        for op in &plan.ops {
            match op {
                NameOp::Rename { from, to } => {
                    w.begin(cmd::RENAME);
                    w.attr(attr::PATH, from)?;
                    w.attr(attr::PATH_TO, to)?;
                }
                NameOp::Unlink(path) => {
                    w.begin(cmd::UNLINK);
                    w.attr(attr::PATH, path)?;
                }
                NameOp::Link { path, existing } => {
                    w.begin(cmd::LINK);
                    w.attr(attr::PATH, path)?;
                    w.attr(attr::PATH_LINK, existing)?;
                }
                NameOp::Create { path, id } => {
                    self.send_create(&ctree, &mut w, path, &c.inodes[id])?;
                }
                NameOp::Rmdir(path) => {
                    w.begin(cmd::RMDIR);
                    w.attr(attr::PATH, path)?;
                }
            }
        }
        let NamePlan {
            ns,
            mut touched,
            created,
            ..
        } = plan;

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

    /// The receiver, as far as names go: what each path names, and
    /// whether it is a directory. Applies each change as `btrfs receive`
    /// would, refusing one the kernel would refuse.
    #[derive(Default)]
    struct Receiver {
        paths: BTreeMap<Vec<u8>, (Id, bool)>,
    }

    fn parent_of(path: &[u8]) -> &[u8] {
        path.iter()
            .rposition(|&b| b == b'/')
            .map_or(&b""[..], |i| &path[..i])
    }

    fn under(path: &[u8], dir: &[u8]) -> bool {
        path.len() > dir.len() && path.starts_with(dir) && path[dir.len()] == b'/'
    }

    impl Receiver {
        fn is_dir(&self, path: &[u8]) -> bool {
            path.is_empty() || self.paths.get(path).is_some_and(|(_, d)| *d)
        }

        fn apply(&mut self, op: &NameOp, dirs: &BTreeSet<Id>) {
            let shown = |p: &[u8]| String::from_utf8_lossy(p).into_owned();
            match op {
                NameOp::Rename { from, to } => {
                    assert!(
                        self.paths.contains_key(from),
                        "rename of {}, which is not there",
                        shown(from)
                    );
                    assert!(
                        !self.paths.contains_key(to),
                        "rename onto {}, which is taken",
                        shown(to)
                    );
                    assert!(
                        self.is_dir(parent_of(to)),
                        "rename into {}, no directory",
                        shown(to)
                    );
                    assert!(!under(to, from), "rename of {} into itself", shown(from));
                    let moved: Vec<Vec<u8>> = self
                        .paths
                        .keys()
                        .filter(|p| *p == from || under(p, from))
                        .cloned()
                        .collect();
                    for p in moved {
                        let v = self.paths.remove(&p).unwrap();
                        let mut q = to.clone();
                        q.extend_from_slice(&p[from.len()..]);
                        self.paths.insert(q, v);
                    }
                }
                NameOp::Unlink(p) => {
                    let (_, dir) = self.paths.remove(p).expect("unlink of a missing name");
                    assert!(!dir, "unlink of directory {}", shown(p));
                }
                NameOp::Link { path, existing } => {
                    let (id, dir) = self.paths[existing];
                    assert!(!dir, "a hard link to directory {}", shown(existing));
                    assert!(!self.paths.contains_key(path), "link onto {}", shown(path));
                    assert!(self.is_dir(parent_of(path)), "link into {}", shown(path));
                    self.paths.insert(path.clone(), (id, false));
                }
                NameOp::Create { path, id } => {
                    assert!(
                        !self.paths.contains_key(path),
                        "create onto {}",
                        shown(path)
                    );
                    assert!(self.is_dir(parent_of(path)), "create in {}", shown(path));
                    self.paths.insert(path.clone(), (*id, dirs.contains(id)));
                }
                NameOp::Rmdir(p) => {
                    let (_, dir) = self.paths.remove(p).expect("rmdir of a missing name");
                    assert!(dir, "rmdir of file {}", shown(p));
                    assert!(
                        !self.paths.keys().any(|q| under(q, p)),
                        "rmdir of {}, which is not empty",
                        shown(p)
                    );
                }
            }
        }
    }

    /// A tree given as path -> inode, as `(directory, name, inode)`
    /// breadth-first.
    fn entries(tree: &BTreeMap<Vec<u8>, Id>) -> Vec<(Id, Vec<u8>, Id)> {
        let mut paths: Vec<&Vec<u8>> = tree.keys().collect();
        paths.sort_by_key(|p| (p.iter().filter(|&&b| b == b'/').count(), (*p).clone()));
        paths
            .into_iter()
            .map(|p| {
                let parent = parent_of(p);
                let dir = if parent.is_empty() { TOP } else { tree[parent] };
                let name = p[p.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1)..].to_vec();
                (dir, name, tree[p])
            })
            .collect()
    }

    /// Plan the parent into the child, replay the plan, and land on the
    /// child.
    fn check(parent: &BTreeMap<Vec<u8>, Id>, child: &BTreeMap<Vec<u8>, Id>, dirs: &BTreeSet<Id>) {
        let kept: BTreeSet<Id> = child.values().copied().collect();
        let plan = plan_names(
            &entries(parent),
            &entries(child),
            |id| dirs.contains(&id),
            |id| kept.contains(&id),
        )
        .unwrap();
        let mut rx = Receiver::default();
        for (p, id) in parent {
            rx.paths.insert(p.clone(), (*id, dirs.contains(id)));
        }
        for op in &plan.ops {
            rx.apply(op, dirs);
        }
        let landed: BTreeMap<Vec<u8>, Id> =
            rx.paths.into_iter().map(|(p, (id, _))| (p, id)).collect();
        assert_eq!(&landed, child, "after {:?}", plan.ops);
    }

    fn tree(paths: &[(&str, u64)]) -> BTreeMap<Vec<u8>, Id> {
        paths
            .iter()
            .map(|(p, ino)| (p.as_bytes().to_vec(), (*ino, 1)))
            .collect()
    }

    /// Swapped names, nesting turned inside out, a file rescued from a
    /// directory that goes, a name given to a new inode, links added and
    /// dropped.
    #[test]
    fn the_hard_shapes_land_on_the_child() {
        let dirs: BTreeSet<Id> = [257, 258, 259, 260, 270].map(|i| (i, 1)).into();
        let parent = tree(&[
            ("a", 257),
            ("a/b", 258),
            ("a/b/x", 300),
            ("gone", 259),
            ("gone/deeper", 260),
            ("gone/deeper/rescued", 301),
            ("s1", 302),
            ("s2", 303),
            ("two", 304),
            ("two-again", 304),
            ("replaced", 305),
        ]);
        let child = tree(&[
            ("b", 258),
            ("b/a", 257),
            ("b/x", 300),
            ("a", 306),
            ("rescued", 301),
            ("s1", 303),
            ("s2", 302),
            ("two", 304),
            ("elsewhere", 270),
            ("elsewhere/two", 304),
            ("replaced", 307),
        ]);
        check(&parent, &child, &dirs);
    }

    /// A small pseudo-random generator, so the shapes below are the same on
    /// every run.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n.max(1) as u64) as usize
        }
    }

    /// A tree being edited at random, as path -> inode.
    struct Edits {
        rng: Rng,
        tree: BTreeMap<Vec<u8>, Id>,
        dirs: BTreeSet<Id>,
        next_ino: u64,
        names: u32,
        /// Numbers of inodes gone since the parent was taken, for reuse
        /// under a new generation.
        reusable: Vec<u64>,
    }

    impl Edits {
        fn fresh_name(&mut self, dir: &[u8]) -> Vec<u8> {
            self.names += 1;
            let mut p = dir.to_vec();
            if !p.is_empty() {
                p.push(b'/');
            }
            p.extend_from_slice(format!("n{}", self.names).as_bytes());
            p
        }

        fn some_dir(&mut self, not_under: Option<&[u8]>) -> Vec<u8> {
            let dirs: Vec<Vec<u8>> = std::iter::once(Vec::new())
                .chain(
                    self.tree
                        .iter()
                        .filter(|(_, id)| self.dirs.contains(id))
                        .map(|(p, _)| p.clone()),
                )
                .filter(|d| not_under.is_none_or(|n| d != n && !under(d, n)))
                .collect();
            dirs[self.rng.below(dirs.len())].clone()
        }

        fn some_path(&mut self, files_only: bool) -> Option<Vec<u8>> {
            let paths: Vec<Vec<u8>> = self
                .tree
                .iter()
                .filter(|(_, id)| !files_only || !self.dirs.contains(id))
                .map(|(p, _)| p.clone())
                .collect();
            (!paths.is_empty()).then(|| paths[self.rng.below(paths.len())].clone())
        }

        fn create(&mut self, reuse: bool) {
            let dir = self.some_dir(None);
            let path = self.fresh_name(&dir);
            let id = if reuse && !self.reusable.is_empty() && self.rng.below(2) == 0 {
                let i = self.rng.below(self.reusable.len());
                (self.reusable.swap_remove(i), 2)
            } else {
                self.next_ino += 1;
                (self.next_ino, 1)
            };
            if self.rng.below(3) == 0 {
                self.dirs.insert(id);
            }
            self.tree.insert(path, id);
        }

        /// Everything at or under `path`.
        fn subtree(&self, path: &[u8]) -> Vec<Vec<u8>> {
            self.tree
                .keys()
                .filter(|q| q.as_slice() == path || under(q, path))
                .cloned()
                .collect()
        }

        fn edit(&mut self) {
            let Some(path) = self.some_path(false) else {
                return self.create(true);
            };
            match self.rng.below(5) {
                0 => self.create(true),
                // Move anything somewhere it can go.
                1 => {
                    let dir = self.some_dir(Some(&path));
                    let to = self.fresh_name(&dir);
                    for q in self.subtree(&path) {
                        let id = self.tree.remove(&q).expect("listed");
                        let mut r = to.clone();
                        r.extend_from_slice(&q[path.len()..]);
                        self.tree.insert(r, id);
                    }
                }
                // Remove a name and everything under it.
                2 => {
                    let before: BTreeSet<u64> = self.tree.values().map(|id| id.0).collect();
                    for q in self.subtree(&path) {
                        self.tree.remove(&q);
                    }
                    let after: BTreeSet<u64> = self.tree.values().map(|id| id.0).collect();
                    self.reusable.extend(before.difference(&after));
                }
                // Another name for a file.
                3 => {
                    if let Some(file) = self.some_path(true) {
                        let dir = self.some_dir(None);
                        let to = self.fresh_name(&dir);
                        let id = self.tree[&file];
                        self.tree.insert(to, id);
                    }
                }
                // A file's name given to a new inode.
                _ => {
                    if let Some(file) = self.some_path(true) {
                        self.next_ino += 1;
                        self.tree.insert(file, (self.next_ino, 1));
                    }
                }
            }
        }
    }

    /// Random trees changed at random -- moves, removals, creations, hard
    /// links, names given to new inodes, inode numbers reused under a new
    /// generation -- all land on the child.
    #[test]
    fn random_changes_land_on_the_child() {
        for seed in 1..=500u64 {
            let mut e = Edits {
                rng: Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1),
                tree: BTreeMap::new(),
                dirs: BTreeSet::new(),
                next_ino: 256,
                names: 0,
                reusable: Vec::new(),
            };
            for _ in 0..15 {
                e.create(false);
            }
            let parent = e.tree.clone();
            for _ in 0..12 {
                e.edit();
            }
            check(&parent, &e.tree, &e.dirs);
        }
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
