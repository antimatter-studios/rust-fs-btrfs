//! The three API layers of rust-fs-core (`fs_core::layers`) for Btrfs.
//!
//! [`BtrfsVolume`] is the volume layer ([`VolumeAPI`], [`OpenVolume`]), the
//! primitive layer ([`PrimitiveAPI`], with [`BtrfsAdapter`] as its
//! [`SalvageAdapter`]) and the file layer ([`FileAPI`]). It is built on this
//! crate's own pieces: the damage-tolerant reads of [`crate::salvage`] below
//! the namespace, and [`Filesystem`] for the namespace of a healthy volume.
//! `docs/three-layers.md` maps the rest of the crate's API onto the layers.
//!
//! Nothing here writes. The source is a [`BlockRead`], and the mount the
//! file layer reads through is the crate's read-only mount.
//!
//! One source per volume: a multi-device pool's stripes on another device
//! are named, never read (rust-fs-core#460).

use crate::btree::{Header, Item, LEAF_DATA_OFFSET};
use crate::compression::{decompress, Compression, ShortDecode, MAX_UNCOMPRESSED};
use crate::fs::{Filesystem, EXTENT_DATA_KEY};
use crate::inode::{Inode, INODE_ITEM_KEY, INODE_REF_KEY};
use crate::salvage::{
    self as native, best_copy, build_chunk_map, read_root_items, root_tree_candidates,
    scan_tree_blocks, superblock_copies, walk_tree, BlockFault, CopyVerdict, DamagedBlock,
    ExtentType, FileExtentItem, Layout, RootPointer, SalvagedBlock,
};
use crate::subvol::EMPTY_SUBVOL_DIR_OBJECTID;
use crate::superblock::{
    le16, le64, refuse_unreadable_incompat, super_flags, Superblock, SUPER_INFO_SIZE,
};
use fs_core::layers::{
    check_bounds, ApiError, Candidate, CandidateKind, CandidateState, Capabilities, Damage,
    DamageKind, DamageReport, DirEntry, Encoding, Extent, ExtentWalk, Feature, FileAPI, Geometry,
    Integrity, Metadata, NodeKind, OpenVolume, Parent, ParentClaim, PrimitiveAPI, Probe,
    Recognition, SalvageAdapter, SalvageError, ScanReport, Severity, Sink, Structure,
    StructureRole, SuperblockCopy, Timestamp, Tree, TreeNode, VolumeAPI, VolumeHealth,
    VolumeSupport, WalkEnd, WalkExtent,
};
use fs_core::BlockRead;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::io;
use std::ops::ControlFlow;
use std::sync::{Arc, Mutex};

/// The top-level filesystem tree's objectid.
const FS_TREE: u64 = 5;
/// The first objectid a subvolume tree, or an inode, can have.
const FIRST_FREE: u64 = 256;
/// The last objectid a subvolume tree can have (`BTRFS_LAST_FREE_OBJECTID`).
const LAST_FREE: u64 = u64::MAX - 255;
/// `BTRFS_ROOT_BACKREF_KEY`: a subvolume's name in its parent tree.
const ROOT_BACKREF_KEY: u8 = 144;
/// Bytes of an inline extent item before its data.
const INLINE_HEADER: u64 = 21;

const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;

fn is_fs_tree(objectid: u64) -> bool {
    objectid == FS_TREE || (FIRST_FREE..=LAST_FREE).contains(&objectid)
}

/// Why a layer call failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum LayerError {
    /// A refusal the layers share: out of bounds, not found, not a
    /// directory, no namespace on a volume opened for salvage.
    Api(ApiError),
    /// The source failed a read.
    Device(fs_core::Error),
    /// This crate refused: a mount that did not validate, a feature it
    /// cannot read.
    Btrfs(crate::Error),
    /// No superblock copy decodes, so there is no geometry to salvage by.
    NoGeometry,
}

impl fmt::Display for LayerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api(error) => error.fmt(formatter),
            Self::Device(error) => error.fmt(formatter),
            Self::Btrfs(error) => error.fmt(formatter),
            Self::NoGeometry => formatter.write_str("no superblock copy decodes"),
        }
    }
}

impl std::error::Error for LayerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Api(error) => Some(error),
            Self::Device(error) => Some(error),
            Self::Btrfs(error) => Some(error),
            Self::NoGeometry => None,
        }
    }
}

impl From<ApiError> for LayerError {
    fn from(error: ApiError) -> Self {
        Self::Api(error)
    }
}

impl From<fs_core::Error> for LayerError {
    fn from(error: fs_core::Error) -> Self {
        Self::Device(error)
    }
}

impl From<crate::Error> for LayerError {
    fn from(error: crate::Error) -> Self {
        Self::Btrfs(error)
    }
}

fn uuid_text(uuid: &[u8; 16]) -> String {
    let hex: String = uuid.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn timestamp(t: &crate::inode::Timestamp) -> Timestamp {
    Timestamp {
        seconds: t.sec,
        nanoseconds: t.nsec,
    }
}

fn node_kind(mode: u32) -> NodeKind {
    match mode & S_IFMT {
        S_IFREG => NodeKind::File,
        S_IFDIR => NodeKind::Directory,
        S_IFLNK => NodeKind::Symlink,
        _ => NodeKind::Special,
    }
}

fn metadata_of(inode: &Inode) -> Metadata {
    let kind = node_kind(inode.mode);
    Metadata {
        kind,
        size: if kind == NodeKind::Directory {
            0
        } else {
            inode.size
        },
        allocated: Some(inode.nbytes),
        created: Some(timestamp(&inode.otime)),
        modified: Some(timestamp(&inode.mtime)),
        accessed: Some(timestamp(&inode.atime)),
        mode: Some(inode.mode & 0o7777),
        links: Some(u64::from(inode.nlink)),
        attributes: Some(inode.flags),
    }
}

/// A source read as a whole into memory, for the strict API given only a
/// borrowed source.
struct Owned(Vec<u8>);

impl BlockRead for Owned {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let start = usize::try_from(offset)
            .map_err(|_| fs_core::Error::Io(io::ErrorKind::InvalidInput.into()))?;
        let end = start
            .checked_add(buf.len())
            .filter(|end| *end <= self.0.len())
            .ok_or_else(|| fs_core::Error::Io(io::ErrorKind::UnexpectedEof.into()))?;
        buf.copy_from_slice(&self.0[start..end]);
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.0.len() as u64
    }
}

/// The strict API over a whole source: the volume opens through the
/// crate's validating mount, and every directory and file it presents
/// reads in full. The source is read into memory first, so it is meant for
/// the small images a conformance run builds.
pub fn strict_read_all(source: &dyn BlockRead) -> Result<(), LayerError> {
    let mut bytes = vec![0u8; source.size_bytes() as usize];
    source.read_at(0, &mut bytes)?;
    let volume = BtrfsVolume::open(Owned(bytes))?;
    let mut stack = vec![volume.root()?];
    while let Some(directory) = stack.pop() {
        for entry in volume.read_dir(&directory)? {
            match entry.kind {
                NodeKind::Directory => stack.push(entry.node),
                NodeKind::File => {
                    volume.copy_to(&entry.node, &mut io::sink())?;
                }
                NodeKind::Symlink => {
                    volume.read_link(&entry.node)?;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// A file or directory: the tree it is in and its inode number. The root of
/// a subvolume is `(subvolume id, 256)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BtrfsNode {
    pub tree: u64,
    pub ino: u64,
}

/// A Btrfs volume, through the three layers.
pub struct BtrfsVolume<S> {
    source: Arc<S>,
    copies: Vec<native::SuperblockCopy>,
    /// The copy the geometry came from.
    chosen: usize,
    sb: Superblock,
    /// The mount the file layer reads through; `None` when the volume was
    /// opened for salvage and the mount refused it.
    fs: Option<Filesystem>,
    /// Subvolumes opened through `fs`, by id.
    subvolumes: Mutex<HashMap<u64, Arc<Filesystem>>>,
    /// Why the mount refused, when it did.
    refusal: Option<String>,
    adapter: BtrfsAdapter,
}

impl<S: BlockRead + 'static> BtrfsVolume<S> {
    fn build(source: S, mount_required: bool) -> Result<Self, LayerError> {
        let source = Arc::new(source);
        let copies = superblock_copies(&*source);
        let chosen = best_copy(&copies)
            .or_else(|| copies.iter().find(|c| c.superblock.is_some()))
            .ok_or(LayerError::NoGeometry)?;
        let sb = chosen.superblock.clone().ok_or(LayerError::NoGeometry)?;
        let chosen = chosen.mirror;
        let device: Arc<dyn BlockRead> = source.clone();
        let (fs, refusal) = match Filesystem::mount(device) {
            Ok(fs) => (Some(fs), None),
            Err(error) if mount_required => return Err(error.into()),
            Err(error) => (None, Some(error.to_string())),
        };
        let adapter = BtrfsAdapter::for_source(&*source);
        Ok(Self {
            source,
            copies,
            chosen,
            sb,
            fs,
            subvolumes: Mutex::new(HashMap::new()),
            refusal,
            adapter,
        })
    }

    /// Which superblock copy the geometry came from: 0 for the primary.
    pub fn chosen_superblock(&self) -> usize {
        self.chosen
    }

    /// The crate's own mount, for what the layers do not cover; `None` on a
    /// volume opened for salvage whose mount was refused.
    pub fn filesystem(&self) -> Option<&Filesystem> {
        self.fs.as_ref()
    }

    fn fs(&self) -> Result<&Filesystem, LayerError> {
        self.fs
            .as_ref()
            .ok_or(LayerError::Api(ApiError::Unsupported(
                "the namespace of a volume whose mount was refused",
            )))
    }

    /// Run `read` against the mount of `tree`: the volume's own for the
    /// top-level tree, an opened subvolume otherwise.
    fn in_tree<T>(
        &self,
        tree: u64,
        read: impl FnOnce(&Filesystem) -> crate::Result<T>,
    ) -> Result<T, LayerError> {
        let fs = self.fs()?;
        if tree == FS_TREE {
            return Ok(read(fs)?);
        }
        let mounted = {
            let mut open = self.subvolumes.lock().expect("the subvolume cache");
            match open.get(&tree) {
                Some(found) => Arc::clone(found),
                None => {
                    let mounted = Arc::new(fs.open_subvolume(tree)?);
                    open.insert(tree, Arc::clone(&mounted));
                    mounted
                }
            }
        };
        Ok(read(&mounted)?)
    }
}

/// The integrity a superblock copy's verdict amounts to.
fn copy_integrity(verdict: &CopyVerdict) -> Integrity {
    match verdict {
        CopyVerdict::Valid | CopyVerdict::UnsupportedFeature(_) => Integrity::Verified,
        _ => Integrity::Failed,
    }
}

impl<S: BlockRead + 'static> VolumeAPI for BtrfsVolume<S> {
    type Error = LayerError;
    type Superblock = Superblock;

    fn format(&self) -> &'static str {
        "btrfs"
    }

    fn size_bytes(&self) -> u64 {
        let declared = self.sb.dev_item.total_bytes;
        let size = self.source.size_bytes();
        if declared == 0 {
            size
        } else {
            size.min(declared)
        }
    }

    fn read_volume_bytes(&self, offset: u64, output: &mut [u8]) -> Result<(), LayerError> {
        check_bounds(offset, output.len(), self.size_bytes())?;
        self.source.read_at(offset, output)?;
        Ok(())
    }

    fn geometry(&self) -> Result<Geometry, LayerError> {
        let sector = u64::from(self.sb.sectorsize);
        let declared = self.sb.dev_item.total_bytes;
        Ok(Geometry {
            sector_size: self.sb.sectorsize,
            allocation_unit: sector,
            allocation_units: declared / sector.max(1),
            declared_bytes: declared,
            layout: None,
            label: (!self.sb.label.is_empty()).then(|| self.sb.label.clone()),
            serial: Some(uuid_text(&self.sb.fsid)),
        })
    }

    fn superblock(&self) -> Result<Superblock, LayerError> {
        Ok(self.sb.clone())
    }

    fn superblocks(&self) -> Result<Vec<SuperblockCopy<Superblock>>, LayerError> {
        Ok(self
            .copies
            .iter()
            .filter(|copy| copy.verdict != CopyVerdict::Absent)
            .map(|copy| SuperblockCopy {
                copy: copy.mirror as u32,
                location: Extent::new(copy.offset, SUPER_INFO_SIZE as u64),
                superblock: copy.superblock.clone(),
                integrity: copy_integrity(&copy.verdict),
            })
            .collect())
    }

    fn support(&self) -> Result<VolumeSupport, LayerError> {
        let mut unsupported = Vec::new();
        if let Err(error) = refuse_unreadable_incompat(self.sb.incompat_flags) {
            unsupported.push(error.to_string());
        }
        if let Some(refusal) = &self.refusal {
            unsupported.push(format!("the namespace: {refusal}"));
        }
        Ok(VolumeSupport {
            files: self.fs.is_some(),
            salvage: self.adapter.capabilities(),
            unsupported,
        })
    }

    fn health(&self) -> Result<VolumeHealth, LayerError> {
        Ok(VolumeHealth::from_findings(self.adapter.quick_damage()))
    }
}

impl<S: BlockRead + 'static> OpenVolume for BtrfsVolume<S> {
    type Source = S;

    fn probe(source: &dyn BlockRead) -> Result<Probe, fs_core::Error> {
        if source.size_bytes() < SUPER_INFO_SIZE as u64 + 0x1_0000 {
            return Ok(Probe {
                format: "btrfs",
                recognition: Recognition::No,
                declared_bytes: None,
                detail: "too small to hold a Btrfs superblock".into(),
            });
        }
        let mut primary = vec![0u8; SUPER_INFO_SIZE];
        source.read_at(0x1_0000, &mut primary)?;
        let copies = superblock_copies(source);
        let (recognition, declared, detail) = match best_copy(&copies) {
            Some(copy) => (
                if copy.mirror == 0 {
                    Recognition::Yes
                } else {
                    Recognition::Damaged
                },
                copy.superblock.as_ref().map(|sb| sb.dev_item.total_bytes),
                format!("a valid superblock copy {}", copy.mirror),
            ),
            None => match copies.iter().find(|c| c.superblock.is_some()) {
                Some(copy) => (
                    Recognition::Damaged,
                    copy.superblock.as_ref().map(|sb| sb.dev_item.total_bytes),
                    format!(
                        "superblock copy {} decodes but {:?}",
                        copy.mirror, copy.verdict
                    ),
                ),
                None => (Recognition::No, None, "no Btrfs superblock".into()),
            },
        };
        Ok(Probe {
            format: "btrfs",
            recognition,
            declared_bytes: declared,
            detail,
        })
    }

    fn open(source: S) -> Result<Self, LayerError> {
        Self::build(source, true)
    }

    fn open_for_salvage(source: S) -> Result<Self, LayerError> {
        Self::build(source, false)
    }
}

impl<S: BlockRead + 'static> PrimitiveAPI for BtrfsVolume<S> {
    type Adapter = BtrfsAdapter;
    type Hypothesis = ();

    fn source(&self) -> &dyn BlockRead {
        &*self.source
    }

    fn adapter(&self) -> Result<BtrfsAdapter, SalvageError> {
        if self.adapter.context.is_none() {
            return Err(SalvageError::NoGeometry);
        }
        Ok(self.adapter.clone())
    }

    fn structures(&self) -> Result<Vec<Structure>, SalvageError> {
        let mut found: Vec<Structure> = self
            .copies
            .iter()
            .filter(|copy| copy.verdict != CopyVerdict::Absent)
            .map(|copy| Structure {
                name: if copy.mirror == 0 {
                    "primary superblock".into()
                } else {
                    format!("superblock mirror {}", copy.mirror)
                },
                role: StructureRole::Superblock,
                location: Extent::new(copy.offset, SUPER_INFO_SIZE as u64),
                copy: copy.mirror as u32,
                integrity: copy_integrity(&copy.verdict),
            })
            .collect();
        let Some(context) = &self.adapter.context else {
            return Ok(found);
        };
        let mut roots = vec![
            (
                "chunk tree root",
                StructureRole::AddressMap,
                context.sb.chunk_root,
            ),
            ("root tree root", StructureRole::Directory, context.sb.root),
        ];
        for tree in &context.trees {
            if tree.objectid == FS_TREE {
                roots.push((
                    "filesystem tree root",
                    StructureRole::Directory,
                    tree.root.bytenr,
                ));
            }
        }
        let source = &*self.source;
        let layout = &context.layout;
        let nodesize = u64::from(layout.geometry.nodesize);
        for (name, role, logical) in roots {
            for mirror in 0..layout.copies(logical).max(1) {
                let Ok(runs) = layout.physical_runs(logical, nodesize, mirror) else {
                    continue;
                };
                let Some(&(physical, _)) = runs.first() else {
                    continue;
                };
                let mut bytes = vec![0u8; nodesize as usize];
                let integrity = match source.read_at(physical, &mut bytes) {
                    Ok(()) => {
                        let (_, faults, _) = native::decode_block(
                            &bytes,
                            logical,
                            &layout.geometry,
                            native::Expect::default(),
                        );
                        if faults.is_empty() {
                            Integrity::Verified
                        } else {
                            Integrity::Failed
                        }
                    }
                    Err(_) => Integrity::Failed,
                };
                found.push(Structure {
                    name: name.into(),
                    role,
                    location: Extent::new(physical, nodesize),
                    copy: mirror as u32,
                    integrity,
                });
            }
        }
        Ok(found)
    }
}

impl<S: BlockRead + 'static> FileAPI for BtrfsVolume<S> {
    type Node = BtrfsNode;

    fn root(&self) -> Result<BtrfsNode, LayerError> {
        self.fs()?;
        Ok(BtrfsNode {
            tree: FS_TREE,
            ino: FIRST_FREE,
        })
    }

    fn read_dir(&self, directory: &BtrfsNode) -> Result<Vec<DirEntry<BtrfsNode>>, LayerError> {
        if self.metadata(directory)?.kind != NodeKind::Directory {
            return Err(ApiError::NotADirectory(format!("{directory:?}")).into());
        }
        if directory.ino == EMPTY_SUBVOL_DIR_OBJECTID {
            return Ok(Vec::new());
        }
        let mut entries = Vec::new();
        for entry in self.in_tree(directory.tree, |fs| fs.read_dir(directory.ino))? {
            // An entry naming a subvolume leads into it only when the
            // subvolume's ROOT_REF names this directory and name; otherwise
            // the kernel shows an empty directory, and so does this. That is
            // a snapshot's copy of a subvolume nested in its source.
            let node = if entry.is_inode() {
                BtrfsNode {
                    tree: directory.tree,
                    ino: entry.ino,
                }
            } else {
                match self.in_tree(directory.tree, |fs| {
                    fs.subvolume_behind(directory.ino, &entry)
                })? {
                    Some((_, dirid)) => BtrfsNode {
                        tree: entry.ino,
                        ino: dirid,
                    },
                    None => BtrfsNode {
                        tree: directory.tree,
                        ino: EMPTY_SUBVOL_DIR_OBJECTID,
                    },
                }
            };
            let metadata = self.metadata(&node)?;
            let (name, lossy) = match String::from_utf8(entry.name.clone()) {
                Ok(name) => (name, false),
                Err(_) => (String::from_utf8_lossy(&entry.name).into_owned(), true),
            };
            entries.push(DirEntry {
                name,
                bytes: entry.name,
                lossy,
                kind: metadata.kind,
                size: metadata.size,
                node,
            });
        }
        Ok(entries)
    }

    fn metadata(&self, node: &BtrfsNode) -> Result<Metadata, LayerError> {
        if node.ino == EMPTY_SUBVOL_DIR_OBJECTID {
            // No tree holds this inode: the kernel makes it up, a directory
            // with mode 0755 and one link.
            let mut metadata = Metadata::new(NodeKind::Directory, 0);
            metadata.mode = Some(0o755);
            metadata.links = Some(1);
            return Ok(metadata);
        }
        let inode = self.in_tree(node.tree, |fs| fs.read_inode(node.ino))?;
        Ok(metadata_of(&inode))
    }

    fn read_at(
        &self,
        file: &BtrfsNode,
        offset: u64,
        output: &mut [u8],
    ) -> Result<usize, LayerError> {
        let inode = self.in_tree(file.tree, |fs| fs.read_inode(file.ino))?;
        if node_kind(inode.mode) == NodeKind::Directory {
            return Err(ApiError::IsADirectory(format!("{file:?}")).into());
        }
        self.in_tree(file.tree, |fs| fs.read_at(file.ino, offset, output))
    }

    fn read_link(&self, link: &BtrfsNode) -> Result<Vec<u8>, LayerError> {
        self.in_tree(link.tree, |fs| fs.read_link(link.ino))
    }
}

/// A filesystem tree the adapter salvages from: the top-level tree or a
/// subvolume, with the root the newest readable root tree records.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeRoot {
    pub objectid: u64,
    pub root: RootPointer,
}

/// What the adapter knows about the volume: plain data, no device.
#[derive(Clone, Debug)]
struct Context {
    sb: Superblock,
    copies: Vec<native::SuperblockCopy>,
    layout: Layout,
    chunk_scan_rejected: Vec<native::RejectedChunk>,
    system_array_tail: Option<String>,
    /// The root tree the trees came from, and the ones newer than it that
    /// led nowhere.
    root_tree: Option<RootPointer>,
    failed_root_trees: Vec<RootPointer>,
    trees: Vec<TreeRoot>,
    /// Every leaf the trees reach, by logical address.
    live_leaves: HashSet<u64>,
    /// Damaged blocks met building the context: chunk tree, root trees,
    /// filesystem trees.
    damaged: Vec<(String, DamagedBlock)>,
}

/// The format-specific fields of a Btrfs candidate: an inode item, and the
/// leaf it was found in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BtrfsDetail {
    /// The filesystem tree that owns the leaf: 5, or a subvolume id.
    pub tree: u64,
    pub ino: u64,
    /// The leaf's logical address, as its header names it, and where on the
    /// source this copy of it was found.
    pub leaf: u64,
    pub leaf_physical: u64,
    /// The transaction that wrote the leaf.
    pub leaf_generation: u64,
    pub mode: u32,
    pub nlink: u32,
    /// The inode's generation and last transaction.
    pub generation: u64,
    pub transid: u64,
}

/// Items with the leaf each was read from.
type LeafItems = Vec<(Arc<SalvagedBlock>, Item)>;

/// The salvage adapter: Btrfs format context, never a device.
#[derive(Clone, Debug)]
pub struct BtrfsAdapter {
    context: Option<Box<Context>>,
    /// The superblock copies, kept when none decodes, so the damage report
    /// can say what each was.
    orphan_copies: Vec<native::SuperblockCopy>,
}

impl BtrfsAdapter {
    /// Build the adapter for a source from whatever survives: the newest
    /// valid superblock copy, or one whose checksum fails; the chunk map
    /// from the system chunk array and the chunk tree; and the filesystem
    /// trees of the newest root tree that names them. Reads the source, and
    /// keeps nothing of it but plain data. With no superblock copy that
    /// decodes, every verb fails with `NoGeometry`.
    pub fn for_source(source: &dyn BlockRead) -> Self {
        let copies = superblock_copies(source);
        let Some(sb) = best_copy(&copies)
            .or_else(|| copies.iter().find(|c| c.superblock.is_some()))
            .and_then(|c| c.superblock.clone())
        else {
            return Self {
                context: None,
                orphan_copies: copies,
            };
        };
        let scan = build_chunk_map(source, &sb);
        let mut damaged: Vec<(String, DamagedBlock)> = scan
            .chunk_tree
            .damaged
            .iter()
            .map(|d| ("chunk tree".to_string(), d.clone()))
            .collect();
        let layout = Layout::new(&sb, scan.map.clone());
        let mut failed_root_trees = Vec::new();
        let mut root_tree = None;
        let mut trees: Vec<TreeRoot> = Vec::new();
        for candidate in root_tree_candidates(&copies) {
            let (items, walk) = read_root_items(source, &layout, candidate.tree_root);
            if items.iter().any(|r| r.objectid == FS_TREE) {
                damaged.extend(
                    walk.damaged
                        .into_iter()
                        .map(|d| ("root tree".to_string(), d)),
                );
                let mut by_id: BTreeMap<u64, RootPointer> = BTreeMap::new();
                for item in items.iter().filter(|r| is_fs_tree(r.objectid)) {
                    by_id.insert(item.objectid, item.pointer());
                }
                trees = by_id
                    .into_iter()
                    .map(|(objectid, root)| TreeRoot { objectid, root })
                    .collect();
                root_tree = Some(candidate.tree_root);
                break;
            }
            damaged.extend(
                walk.damaged
                    .into_iter()
                    .map(|d| ("root tree (not used)".to_string(), d)),
            );
            failed_root_trees.push(candidate.tree_root);
        }
        let mut live_leaves = HashSet::new();
        for tree in &trees {
            let report = walk_tree(source, &layout, tree.root, &mut |leaf| {
                live_leaves.insert(leaf.logical);
                ControlFlow::Continue(())
            });
            damaged.extend(
                report
                    .damaged
                    .into_iter()
                    .map(|d| (format!("filesystem tree {}", tree.objectid), d)),
            );
        }
        Self {
            context: Some(Box::new(Context {
                sb,
                copies,
                layout,
                chunk_scan_rejected: scan.rejected,
                system_array_tail: scan.system_array_tail,
                root_tree,
                failed_root_trees,
                trees,
                live_leaves,
                damaged,
            })),
            orphan_copies: Vec::new(),
        }
    }

    fn context(&self) -> Result<&Context, SalvageError> {
        self.context.as_deref().ok_or(SalvageError::NoGeometry)
    }

    /// The filesystem trees the adapter salvages from.
    pub fn trees(&self) -> &[TreeRoot] {
        self.context.as_deref().map_or(&[], |c| &c.trees)
    }

    /// Findings from the superblock copies, the chunk map and every tree
    /// block met building the adapter: what [`VolumeAPI::health`] reports
    /// and the damage report starts with.
    fn quick_damage(&self) -> Vec<Damage> {
        let Some(context) = self.context.as_deref() else {
            return self
                .orphan_copies
                .iter()
                .map(|copy| Damage {
                    kind: match copy.verdict {
                        CopyVerdict::Unreadable(_) => DamageKind::Unreadable,
                        _ => DamageKind::Signature,
                    },
                    severity: Severity::Error,
                    location: Extent::new(copy.offset, SUPER_INFO_SIZE as u64),
                    structure: format!("superblock copy {}", copy.mirror),
                    detail: format!("{:?}; no superblock copy decodes", copy.verdict),
                })
                .collect();
        };
        let mut found = Vec::new();
        let any_valid = context
            .copies
            .iter()
            .any(|c| c.verdict == CopyVerdict::Valid);
        for copy in &context.copies {
            let kind = match &copy.verdict {
                CopyVerdict::Valid | CopyVerdict::Absent | CopyVerdict::UnsupportedFeature(_) => {
                    continue
                }
                CopyVerdict::Unreadable(_) => DamageKind::Unreadable,
                CopyVerdict::NotBtrfs => DamageKind::Signature,
                CopyVerdict::ChecksumMismatch => DamageKind::Checksum,
                CopyVerdict::Misplaced { .. } => DamageKind::Mismatch,
                CopyVerdict::UnknownChecksumType(_) | CopyVerdict::Malformed(_) => {
                    DamageKind::Malformed
                }
            };
            found.push(Damage {
                kind,
                severity: if any_valid {
                    Severity::Warning
                } else {
                    Severity::Error
                },
                location: Extent::new(copy.offset, SUPER_INFO_SIZE as u64),
                structure: format!("superblock copy {}", copy.mirror),
                detail: format!("{:?}", copy.verdict),
            });
        }
        let valid: Vec<&native::SuperblockCopy> = context
            .copies
            .iter()
            .filter(|c| c.verdict == CopyVerdict::Valid)
            .collect();
        if let Some(newest) = valid.iter().filter_map(|c| c.generation()).max() {
            for copy in valid.iter().filter(|c| c.generation() != Some(newest)) {
                found.push(Damage {
                    kind: DamageKind::Mismatch,
                    severity: Severity::Warning,
                    location: Extent::new(copy.offset, SUPER_INFO_SIZE as u64),
                    structure: format!("superblock copy {}", copy.mirror),
                    detail: format!(
                        "generation {:?}, where the newest copy has {newest}",
                        copy.generation()
                    ),
                });
            }
        }
        let primary = Extent::new(0x1_0000, SUPER_INFO_SIZE as u64);
        if context.sb.has_dirty_log() {
            found.push(Damage {
                kind: DamageKind::Recorded,
                severity: Severity::Notice,
                location: primary,
                structure: "superblock".into(),
                detail: format!("an unreplayed log tree at {}", context.sb.log_root),
            });
        }
        if context.sb.flags & super_flags::ERROR != 0 {
            found.push(Damage {
                kind: DamageKind::Recorded,
                severity: Severity::Error,
                location: primary,
                structure: "superblock".into(),
                detail: "the filesystem recorded an error".into(),
            });
        }
        if let Some(tail) = &context.system_array_tail {
            found.push(Damage {
                kind: DamageKind::Malformed,
                severity: Severity::Error,
                location: primary,
                structure: "system chunk array".into(),
                detail: tail.clone(),
            });
        }
        for rejected in &context.chunk_scan_rejected {
            found.push(Damage {
                kind: DamageKind::Malformed,
                severity: Severity::Error,
                location: primary,
                structure: format!("chunk item at logical {}", rejected.logical),
                detail: format!("{:?}: {}", rejected.source, rejected.reason),
            });
        }
        for tree_root in &context.failed_root_trees {
            found.push(Damage {
                kind: DamageKind::Broken,
                severity: if context.root_tree.is_some() {
                    Severity::Warning
                } else {
                    Severity::Error
                },
                location: self.block_extent(tree_root.bytenr, 0),
                structure: format!("root tree at logical {}", tree_root.bytenr),
                detail: format!(
                    "generation {:?} does not lead to the filesystem tree",
                    tree_root.generation
                ),
            });
        }
        for (structure, block) in &context.damaged {
            found.push(self.block_damage(structure, block));
        }
        found
    }

    /// The source bytes copy `mirror` of the tree block at `logical`
    /// occupies, or the primary superblock's when it maps nowhere.
    fn block_extent(&self, logical: u64, mirror: usize) -> Extent {
        let Some(context) = self.context.as_deref() else {
            return Extent::new(0, 0);
        };
        let nodesize = u64::from(context.layout.geometry.nodesize);
        match context.layout.physical_runs(logical, nodesize, mirror) {
            Ok(runs) if !runs.is_empty() => Extent::new(runs[0].0, nodesize),
            _ => Extent::new(0x1_0000, SUPER_INFO_SIZE as u64),
        }
    }

    fn block_damage(&self, structure: &str, block: &DamagedBlock) -> Damage {
        let faults = block
            .attempts
            .iter()
            .flat_map(|a| a.faults.iter())
            .collect::<Vec<_>>();
        let kind = faults
            .iter()
            .map(|f| match f {
                BlockFault::Checksum => DamageKind::Checksum,
                BlockFault::Unreadable(_) => DamageKind::Unreadable,
                BlockFault::Unmapped | BlockFault::OtherDevice { .. } => DamageKind::Missing,
                BlockFault::Malformed(_) => DamageKind::Malformed,
                _ => DamageKind::Mismatch,
            })
            .next()
            .unwrap_or(DamageKind::Broken);
        let failed_copy = block
            .attempts
            .iter()
            .find(|a| !a.faults.is_empty())
            .map_or(0, |a| a.mirror);
        Damage {
            kind,
            severity: if block.faults.is_empty() {
                Severity::Warning
            } else {
                Severity::Error
            },
            location: self.block_extent(block.logical, failed_copy),
            structure: format!("{structure} block at logical {}", block.logical),
            detail: block
                .attempts
                .iter()
                .map(|a| format!("copy {}: {:?}", a.mirror, a.faults))
                .collect::<Vec<_>>()
                .join("; "),
        }
    }

    /// The leaf a candidate was found in, read again from where it was
    /// found.
    fn candidate_leaf(
        &self,
        source: &dyn BlockRead,
        detail: &BtrfsDetail,
    ) -> Result<Option<SalvagedBlock>, SalvageError> {
        let context = self.context()?;
        let nodesize = context.layout.geometry.nodesize as usize;
        let mut bytes = vec![0u8; nodesize];
        if source.read_at(detail.leaf_physical, &mut bytes).is_err() {
            return Ok(None);
        }
        Ok(Some(native::salvaged_from_bytes(
            bytes,
            detail.leaf,
            &context.layout.geometry,
        )))
    }

    /// The items `(ino, key_type, *)` a candidate's tree records: from the
    /// live tree for a live candidate, from its own leaf otherwise. The
    /// second value is the block that could not be read, when one hid
    /// part of the items.
    fn items_of(
        &self,
        source: &dyn BlockRead,
        candidate: &Candidate<BtrfsDetail>,
        key_type: u8,
    ) -> Result<(LeafItems, Option<Extent>), SalvageError> {
        let context = self.context()?;
        let detail = &candidate.detail;
        let live = candidate.state == CandidateState::Live;
        if let (true, Some(tree)) = (
            live,
            context.trees.iter().find(|t| t.objectid == detail.tree),
        ) {
            let (found, report) =
                native::object_items(source, &context.layout, tree.root, detail.ino, key_type);
            let hidden = (!report.complete)
                .then(|| {
                    report
                        .damaged
                        .iter()
                        .find(|d| !d.decoded)
                        .map(|d| self.block_extent(d.logical, 0))
                })
                .flatten();
            let items = found
                .into_iter()
                .map(|(leaf, item)| (Arc::new((*leaf).clone()), item))
                .collect();
            return Ok((items, hidden));
        }
        let Some(leaf) = self.candidate_leaf(source, detail)? else {
            let nodesize = u64::from(context.layout.geometry.nodesize);
            return Ok((
                Vec::new(),
                Some(Extent::new(detail.leaf_physical, nodesize)),
            ));
        };
        let leaf = Arc::new(leaf);
        let items = leaf
            .items()
            .iter()
            .filter(|i| i.key.objectid == detail.ino && i.key.key_type == key_type)
            .map(|i| (Arc::clone(&leaf), *i))
            .collect();
        Ok((items, None))
    }

    /// Where on the source a leaf's copy lies: the physical start of the
    /// copy that was read.
    fn leaf_physical(&self, leaf: &SalvagedBlock) -> Option<u64> {
        let context = self.context.as_deref()?;
        let nodesize = u64::from(context.layout.geometry.nodesize);
        context
            .layout
            .physical_runs(leaf.logical, nodesize, leaf.mirror)
            .ok()?
            .first()
            .map(|run| run.0)
    }
}

fn codec_name(compression: u8) -> Option<&'static str> {
    match compression {
        1 => Some("zlib"),
        2 => Some("lzo"),
        3 => Some("zstd"),
        _ => None,
    }
}

/// The decompressor a codec name, as [`codec_name`] gives it, stands for.
fn codec_compression(codec: &str) -> Option<Compression> {
    match codec {
        "zlib" => Some(Compression::Zlib),
        "lzo" => Some(Compression::Lzo),
        "zstd" => Some(Compression::Zstd),
        _ => None,
    }
}

fn extent_in(source: &dyn BlockRead, extent: Extent) -> Result<(), SalvageError> {
    match extent.end() {
        Some(end) if end <= source.size_bytes() => Ok(()),
        _ => Err(SalvageError::Bounds),
    }
}

fn decode_failure(message: String) -> SalvageError {
    SalvageError::Device(fs_core::Error::Io(io::Error::new(
        io::ErrorKind::InvalidData,
        message,
    )))
}

impl SalvageAdapter for BtrfsAdapter {
    type Detail = BtrfsDetail;

    fn capabilities(&self) -> Capabilities {
        // Btrfs records no deletion: a removed file's older records are found
        // as candidates of unknown state, never as deleted ones. Addresses map
        // through chunks, so there is no cluster numbering.
        Capabilities {
            missing: vec![
                Feature::DeletedCandidates,
                Feature::DeletedExtents,
                Feature::ClusterSteps,
                Feature::GeometryHypotheses,
            ],
        }
    }

    fn find_candidates(
        &self,
        source: &dyn BlockRead,
        range: Extent,
        sink: &mut dyn Sink<Candidate<BtrfsDetail>>,
    ) -> Result<ScanReport, SalvageError> {
        extent_in(source, range)?;
        let Some(context) = self.context.as_deref() else {
            // No geometry because nothing could be read: the scan names the
            // range unreadable rather than refusing it.
            let unreadable = !self.orphan_copies.is_empty()
                && self
                    .orphan_copies
                    .iter()
                    .all(|c| matches!(c.verdict, CopyVerdict::Unreadable(_) | CopyVerdict::Absent));
            if unreadable {
                return Ok(ScanReport {
                    candidates: 0,
                    unreadable: if range.length == 0 {
                        Vec::new()
                    } else {
                        vec![range]
                    },
                    stopped_at: None,
                });
            }
            return Err(SalvageError::NoGeometry);
        };
        let layout = &context.layout;
        let nodesize = u64::from(layout.geometry.nodesize);
        let start = range.offset / nodesize * nodesize;
        let end = range.end().ok_or(SalvageError::Bounds)?;
        let mut report = ScanReport::default();
        let mut stop: Option<u64> = None;
        // A record that starts inside the range may lie in a node that runs
        // past its end, so whole nodes are scanned.
        let scan_end = end
            .div_ceil(nodesize)
            .saturating_mul(nodesize)
            .min(source.size_bytes());
        let summary = scan_tree_blocks(source, layout, start, scan_end - start, &mut |found| {
            let header: &Header = &found.header;
            if header.level != 0 || !is_fs_tree(header.owner) {
                return ControlFlow::Continue(());
            }
            let mut bytes = vec![0u8; nodesize as usize];
            if source.read_at(found.physical, &mut bytes).is_err() {
                return ControlFlow::Continue(());
            }
            // A second copy of a leaf whose first copy verifies is the same
            // record again: one candidate is enough.
            if found.is_consistent() {
                if let Ok(primary) = layout.chunks.map(header.bytenr) {
                    if primary.physical != found.physical {
                        let mut first = vec![0u8; nodesize as usize];
                        if source.read_at(primary.physical, &mut first).is_ok()
                            && native::decode_block(
                                &first,
                                header.bytenr,
                                &layout.geometry,
                                native::Expect::default(),
                            )
                            .1
                            .is_empty()
                        {
                            return ControlFlow::Continue(());
                        }
                    }
                }
            }
            let leaf = native::salvaged_from_bytes(bytes, header.bytenr, &layout.geometry);
            let verified = leaf.is_verified() && found.is_consistent();
            let live = verified && context.live_leaves.contains(&header.bytenr);
            // Item data fills a leaf from its end, so the records run
            // backwards; candidates stream in source order.
            let mut items: Vec<&Item> = leaf.items().iter().collect();
            items.sort_by_key(|item| item.offset);
            for item in items {
                if item.key.key_type != INODE_ITEM_KEY {
                    continue;
                }
                let location = Extent::new(
                    found.physical + LEAF_DATA_OFFSET as u64 + u64::from(item.offset),
                    u64::from(item.size),
                );
                if location.offset < range.offset || location.offset >= end {
                    continue;
                }
                let Some(data) = leaf.item_data(item) else {
                    continue;
                };
                let Ok(inode) = Inode::parse(data, item.key.objectid) else {
                    continue;
                };
                let name = leaf
                    .items()
                    .iter()
                    .find(|i| {
                        i.key.objectid == item.key.objectid && i.key.key_type == INODE_REF_KEY
                    })
                    .and_then(|i| leaf.item_data(i))
                    .and_then(|data| {
                        let len = le16(data, 8) as usize;
                        data.get(10..10 + len)
                            .map(|n| String::from_utf8_lossy(n).into_owned())
                    });
                let kind = match node_kind(inode.mode) {
                    NodeKind::File => CandidateKind::File,
                    NodeKind::Directory => CandidateKind::Directory,
                    _ => CandidateKind::Other,
                };
                let candidate = Candidate {
                    location,
                    kind,
                    state: if live {
                        CandidateState::Live
                    } else {
                        CandidateState::Unknown
                    },
                    integrity: if verified {
                        Integrity::Verified
                    } else {
                        Integrity::Failed
                    },
                    name,
                    size: Some(if kind == CandidateKind::Directory {
                        0
                    } else {
                        inode.size
                    }),
                    detail: BtrfsDetail {
                        tree: header.owner,
                        ino: item.key.objectid,
                        leaf: header.bytenr,
                        leaf_physical: found.physical,
                        leaf_generation: header.generation,
                        mode: inode.mode,
                        nlink: inode.nlink,
                        generation: inode.generation,
                        transid: inode.transid,
                    },
                };
                report.candidates += 1;
                if sink.accept(candidate).is_break() {
                    stop = Some(location.offset + 1);
                    return ControlFlow::Break(());
                }
            }
            ControlFlow::Continue(())
        });
        report.unreadable = summary
            .unreadable
            .iter()
            .map(|&(offset, length)| Extent::new(offset, length))
            .collect();
        report.stopped_at = stop;
        Ok(report)
    }

    fn walk_extents(
        &self,
        source: &dyn BlockRead,
        candidate: &Candidate<BtrfsDetail>,
    ) -> Result<ExtentWalk, SalvageError> {
        if candidate.state == CandidateState::Deleted {
            return Err(SalvageError::Unsupported(Feature::DeletedExtents));
        }
        let context = self.context()?;
        let layout = &context.layout;
        let detail = &candidate.detail;
        let kind = node_kind(detail.mode);
        let claimed = match kind {
            NodeKind::File | NodeKind::Symlink => candidate.size.unwrap_or(0),
            _ => 0,
        };
        if claimed == 0 {
            return Ok(ExtentWalk {
                extents: Vec::new(),
                claimed,
                end: WalkEnd::Complete,
            });
        }
        let (items, hidden) = self.items_of(source, candidate, EXTENT_DATA_KEY)?;
        let mut extents: Vec<WalkExtent> = Vec::new();
        let mut end = WalkEnd::Complete;
        let mut covered = 0u64;
        for (leaf, item) in &items {
            let Some(data) = leaf.item_data(item) else {
                continue;
            };
            let Some(extent) = FileExtentItem::parse(item.key.offset, data) else {
                end = WalkEnd::Broken;
                break;
            };
            let at = extent.file_offset;
            if at >= claimed || at < covered {
                continue;
            }
            let length = extent.num_bytes.min(claimed - at);
            if length == 0 {
                continue;
            }
            match extent.kind {
                ExtentType::Inline => {
                    let Some(physical) = self
                        .leaf_physical(leaf)
                        .or_else(|| (leaf.logical == detail.leaf).then_some(detail.leaf_physical))
                    else {
                        end = WalkEnd::Broken;
                        break;
                    };
                    let stored = Extent::new(
                        physical + LEAF_DATA_OFFSET as u64 + u64::from(item.offset) + INLINE_HEADER,
                        extent.inline.len() as u64,
                    );
                    let encoded = codec_name(extent.compression).map(|name| Encoding {
                        codec: name.to_owned(),
                        decoded: extent.ram_bytes,
                        offset: 0,
                        length,
                    });
                    let stored = if encoded.is_none() {
                        Extent::new(stored.offset, stored.length.min(length))
                    } else {
                        stored
                    };
                    extents.push(WalkExtent {
                        offset: at,
                        source: stored,
                        uninitialized: false,
                        encoded,
                    });
                }
                ExtentType::Regular if extent.disk_bytenr == 0 => {}
                ExtentType::Regular | ExtentType::Prealloc => {
                    let compressed = extent.compression != 0 && extent.kind == ExtentType::Regular;
                    let (logical, span) = if compressed {
                        (extent.disk_bytenr, extent.disk_num_bytes)
                    } else {
                        (extent.disk_bytenr + extent.offset, length)
                    };
                    let runs = match layout.physical_runs(logical, span, 0) {
                        Ok(runs) => runs,
                        Err(_) => {
                            end = WalkEnd::Broken;
                            break;
                        }
                    };
                    if compressed {
                        let (Some(name), [(physical, stored)]) =
                            (codec_name(extent.compression), runs.as_slice())
                        else {
                            end = WalkEnd::Broken;
                            break;
                        };
                        extents.push(WalkExtent {
                            offset: at,
                            source: Extent::new(*physical, *stored),
                            uninitialized: false,
                            encoded: Some(Encoding {
                                codec: name.to_owned(),
                                decoded: extent.ram_bytes,
                                offset: extent.offset,
                                length,
                            }),
                        });
                    } else {
                        let mut file = at;
                        for (physical, run) in runs {
                            extents.push(WalkExtent {
                                offset: file,
                                source: Extent::new(physical, run),
                                uninitialized: extent.kind == ExtentType::Prealloc,
                                encoded: None,
                            });
                            file += run;
                        }
                    }
                }
            }
            covered = at + length;
        }
        if let (WalkEnd::Complete, Some(hidden)) = (end, hidden) {
            end = WalkEnd::Unreadable(hidden);
        }
        // Merge stored runs that continue each other in the file and on the
        // source; encoded runs never merge.
        let mut merged: Vec<WalkExtent> = Vec::with_capacity(extents.len());
        for extent in extents {
            if let Some(last) = merged.last_mut() {
                if last.encoded.is_none()
                    && extent.encoded.is_none()
                    && last.uninitialized == extent.uninitialized
                    && last.end() == Some(extent.offset)
                    && last.source.end() == Some(extent.source.offset)
                {
                    last.source.length += extent.source.length;
                    continue;
                }
            }
            merged.push(extent);
        }
        Ok(ExtentWalk {
            extents: merged,
            claimed,
            end,
        })
    }

    fn rebuild_tree(
        &self,
        source: &dyn BlockRead,
        candidates: &[Candidate<BtrfsDetail>],
    ) -> Result<Tree, SalvageError> {
        let context = self.context()?;
        // A parent by (tree, ino): the live candidate, else the first one.
        // The same inode can be found more than once: copies of a leaf, and
        // older generations. The parent is the strongest: live, then
        // verified, then the lowest source offset, whatever the input order.
        let rank = |c: &Candidate<BtrfsDetail>| {
            (
                c.state != CandidateState::Live,
                c.integrity != Integrity::Verified,
                c.location.offset,
            )
        };
        let mut best: HashMap<(u64, u64), &Candidate<BtrfsDetail>> = HashMap::new();
        for candidate in candidates {
            let key = (candidate.detail.tree, candidate.detail.ino);
            match best.get(&key) {
                Some(current) if rank(current) <= rank(candidate) => {}
                _ => {
                    best.insert(key, candidate);
                }
            }
        }
        let by_inode: HashMap<(u64, u64), u64> = best
            .into_iter()
            .map(|(key, c)| (key, c.location.offset))
            .collect();
        // Subvolume roots hang off a directory in another tree, named by the
        // root tree's ROOT_BACKREF items.
        let mut backrefs: HashMap<u64, Vec<(u64, u64, Vec<u8>)>> = HashMap::new();
        if let Some(root_tree) = context.root_tree {
            walk_tree(source, &context.layout, root_tree, &mut |leaf| {
                for item in leaf.items() {
                    if item.key.key_type != ROOT_BACKREF_KEY {
                        continue;
                    }
                    let Some(data) = leaf.item_data(item) else {
                        continue;
                    };
                    if data.len() < 18 {
                        continue;
                    }
                    let dirid = le64(data, 0);
                    let len = le16(data, 16) as usize;
                    let name = data.get(18..18 + len).map(<[u8]>::to_vec);
                    backrefs.entry(item.key.objectid).or_default().push((
                        item.key.offset,
                        dirid,
                        name.unwrap_or_default(),
                    ));
                }
                ControlFlow::Continue(())
            });
        }
        let mut nodes = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let detail = &candidate.detail;
            let mut parents = Vec::new();
            if detail.ino == FIRST_FREE {
                if detail.tree == FS_TREE {
                    parents.push(ParentClaim::live(Parent::Root));
                } else {
                    for (parent_tree, dirid, name) in
                        backrefs.get(&detail.tree).into_iter().flatten()
                    {
                        if let Some(&parent) = by_inode.get(&(*parent_tree, *dirid)) {
                            parents.push(ParentClaim {
                                parent: Parent::Candidate(parent),
                                name: Some(name.clone()),
                                state: CandidateState::Live,
                            });
                        }
                    }
                }
            } else {
                let (refs, _) = self.items_of(source, candidate, INODE_REF_KEY)?;
                for (leaf, item) in refs {
                    let Some(data) = leaf.item_data(&item) else {
                        continue;
                    };
                    let parent_ino = item.key.offset;
                    let mut at = 0usize;
                    while at + 10 <= data.len() {
                        let len = le16(data, at + 8) as usize;
                        let Some(name) = data.get(at + 10..at + 10 + len) else {
                            break;
                        };
                        if let Some(&parent) = by_inode.get(&(detail.tree, parent_ino)) {
                            parents.push(ParentClaim {
                                parent: Parent::Candidate(parent),
                                name: Some(name.to_vec()),
                                state: if leaf.is_verified() {
                                    candidate.state
                                } else {
                                    CandidateState::Unknown
                                },
                            });
                        }
                        at += 10 + len;
                    }
                }
            }
            nodes.push(TreeNode {
                candidate: candidate.location.offset,
                parents,
            });
        }
        Ok(Tree { nodes })
    }

    fn report_damage(
        &self,
        source: &dyn BlockRead,
        sink: &mut dyn Sink<Damage>,
    ) -> Result<DamageReport, SalvageError> {
        let mut report = DamageReport::default();
        let mut findings = self.quick_damage();
        if let Ok(context) = self.context() {
            // The trees the adapter did not walk to build itself.
            if let Some(root_tree) = context.root_tree {
                let (items, _) = read_root_items(source, &context.layout, root_tree);
                for item in items
                    .iter()
                    .filter(|r| !is_fs_tree(r.objectid) && r.key_offset == 0)
                {
                    let walk = walk_tree(source, &context.layout, item.pointer(), &mut |_| {
                        ControlFlow::Continue(())
                    });
                    for block in &walk.damaged {
                        findings.push(self.block_damage(&format!("tree {}", item.objectid), block));
                    }
                }
            }
        }
        for finding in findings {
            report.reported += 1;
            if sink.accept(finding).is_break() {
                report.stopped = true;
                break;
            }
        }
        Ok(report)
    }

    fn read_extent(
        &self,
        source: &dyn BlockRead,
        extent: Extent,
        offset: u64,
        output: &mut [u8],
    ) -> Result<usize, SalvageError> {
        extent_in(source, extent)?;
        if offset > extent.length {
            return Err(SalvageError::Bounds);
        }
        let count = (extent.length - offset).min(output.len() as u64) as usize;
        if count == 0 {
            return Ok(0);
        }
        source
            .read_at(extent.offset + offset, &mut output[..count])
            .map_err(SalvageError::Device)?;
        Ok(count)
    }

    fn read_encoded(
        &self,
        source: &dyn BlockRead,
        stored: Extent,
        encoding: &Encoding,
        offset: u64,
        output: &mut [u8],
    ) -> Result<usize, SalvageError> {
        let Some(algo) = codec_compression(&encoding.codec) else {
            return Err(SalvageError::Unsupported(Feature::EncodedExtents));
        };
        extent_in(source, stored)?;
        if offset > encoding.length {
            return Err(SalvageError::Bounds);
        }
        let count = (encoding.length - offset).min(output.len() as u64) as usize;
        if count == 0 {
            return Ok(0);
        }
        let context = self.context()?;
        let mut raw = vec![0u8; stored.length as usize];
        source
            .read_at(stored.offset, &mut raw)
            .map_err(SalvageError::Device)?;
        // A Btrfs reference covers `decoded[offset .. offset + length]` of
        // what its extent decodes to; the extent's last unit may decode short
        // of `decoded`, and the rest reads as zeros, as the kernel reads it.
        let ram = (encoding.decoded as usize).min(MAX_UNCOMPRESSED);
        let decoded = decompress(
            algo,
            &raw,
            ram,
            context.layout.sectorsize as usize,
            ShortDecode::Pad,
        )
        .map_err(|error| decode_failure(error.to_string()))?;
        let from = (encoding.offset + offset) as usize;
        let window = decoded.get(from..from + count).ok_or_else(|| {
            decode_failure(format!(
                "{} decoded bytes, short of {}",
                decoded.len(),
                from + count
            ))
        })?;
        output[..count].copy_from_slice(window);
        Ok(count)
    }
}
