//! Mounted filesystem handle.
//!
//! Ties the superblock, chunk map, B-tree reader and item parsers into
//! the operations a consumer wants: open a device, resolve a path, list
//! a directory, read a file.
//!
//! # Getting to the data
//!
//! Btrfs does not name the fs tree in its superblock, so mounting is a
//! four-step bootstrap and every step must succeed before the next is
//! even addressable:
//!
//! 1. Parse the superblock at 64 KiB.
//! 2. Build a chunk map from the `sys_chunk_array` embedded in it —
//!    enough to reach the chunk tree and nothing else.
//! 3. Walk the chunk tree through that partial map, folding every chunk
//!    item in to complete it.
//! 4. Read the root tree, find the `ROOT_ITEM` for the fs tree, and take
//!    the tree's root address from it.
//!
//! # What is deliberately refused
//!
//! Returning plausible-but-wrong file contents is the one failure a
//! caller cannot detect, so these are errors rather than best efforts:
//!
//! Compressed extents are NOT in that list. All three btrfs codecs —
//! zlib, LZO and zstd — are decoded, in [`crate::compression`]. This
//! module used to say decompression was unimplemented, which was true
//! when it was written and has not been for some time.
//!
//! - **Encoded extents** — encrypted or otherwise transformed.
//! - **A dirty log.** `log_root` being set means the tree on disk is not
//!   the whole story.
//!
//! Holes read as zeros, which is what they are.

use crate::btree::{Tree, TreeGeometry};
use crate::chunk::{Chunk, ChunkMap, DiskKey};
use crate::compression::{self, Compression};
use crate::dir::{self, DirEntry, DIR_INDEX_KEY};
use crate::error::{Error, Result};
use crate::inode::{Inode, FIRST_FREE_OBJECTID, INODE_ITEM_KEY};
use crate::superblock::{le64, Superblock, SUPER_INFO_OFFSET};
use fs_core::{BlockDevice, BlockRead};
use std::collections::BTreeMap;
use std::sync::Arc;

/// `BTRFS_FS_TREE_OBJECTID` — the subvolume holding the default
/// filesystem namespace.
pub const FS_TREE_OBJECTID: u64 = 5;

/// One item of the root tree: `(objectid, key_type, offset, data)`.
///
/// Named because the tuple is what the root tree actually holds — a key
/// in three parts and an opaque body whose meaning depends on the type —
/// and a struct here would invent a shape the format does not have.
pub type RootTreeItem = (u64, u8, u64, Vec<u8>);

/// `BTRFS_ROOT_ITEM_KEY`.
pub const ROOT_ITEM_KEY: u8 = 132;

/// `BTRFS_EXTENT_DATA_KEY` — a file's data, inline or by reference.
pub const EXTENT_DATA_KEY: u8 = 108;

/// Byte offsets within `struct btrfs_root_item`.
///
/// The item opens with an embedded `btrfs_inode_item` of 160 bytes,
/// followed by `generation` and `root_dirid` before the root address.
///
/// THE one copy. There were three — here, in `subvol`, and in
/// `transaction` — and only one of them carried the note below about
/// offset 160, which is the field a writer gets wrong.
pub mod root_item {
    /// `u64`. The transaction this tree was last written in.
    ///
    /// AT 160, after the embedded `btrfs_inode_item`. Offset 16 is
    /// inside that inode and holds something else entirely — a
    /// `ROOT_ITEM` whose generation was written there leaves the real
    /// field stale, and the kernel refuses the tree it names with
    /// "parent transid verify failed". Which is exactly what `btrfs
    /// check` said before this was measured.
    pub const GENERATION: usize = 160;
    /// `u64`. The tree's root block.
    ///
    /// Measured against a real filesystem, not counted from the struct:
    /// the `ROOT_ITEM` for the extent tree holds the address the
    /// superblock's own walk reaches. A wrong value here yields an
    /// address whose tree block fails its own identity check rather
    /// than producing plausible garbage.
    pub const BYTENR: usize = 176;
    /// `u64`. The generation this subvolume was last snapshotted at, or
    /// zero if it never was.
    pub const LAST_SNAPSHOT: usize = 200;
    /// `u64`. Bit 0 is `BTRFS_ROOT_SUBVOL_RDONLY`.
    pub const FLAGS: usize = 208;
    /// The height of the tree this item names.
    pub const LEVEL: usize = 238;
    /// The smallest item any of these fields can be read out of.
    pub const MIN_SIZE: usize = FLAGS + 8;
}

/// Byte offsets within `struct btrfs_file_extent_item`.
///
/// The full field list is kept even where a read-only driver does not
/// consult every one, because the offsets that follow are only checkable
/// against the format documentation when the fields between them are
/// named too.
#[allow(dead_code)]
mod file_extent {
    /// Generation of the transaction that created it.
    pub const GENERATION: usize = 0;
    /// Decoded size of the extent's data.
    pub const RAM_BYTES: usize = 8;
    /// Compression algorithm, 0 for none.
    pub const COMPRESSION: usize = 16;
    /// Encryption, 0 for none.
    pub const ENCRYPTION: usize = 17;
    /// Other encoding, 0 for none.
    pub const OTHER_ENCODING: usize = 18;
    /// 0 = inline, 1 = regular, 2 = prealloc.
    pub const TYPE: usize = 20;
    /// Inline data begins here.
    pub const INLINE_DATA: usize = 21;
    /// Physical address of the extent, or 0 for a hole.
    pub const DISK_BYTENR: usize = 21;
    /// Bytes occupied on disk.
    pub const DISK_NUM_BYTES: usize = 29;
    /// Offset into the extent at which this reference starts.
    pub const OFFSET: usize = 37;
    /// Logical length of this reference.
    pub const NUM_BYTES: usize = 45;
    /// Size of the non-inline header.
    pub const REGULAR_SIZE: usize = 53;
}

/// Extent storage kinds.
const EXTENT_INLINE: u8 = 0;
const EXTENT_REGULAR: u8 = 1;
const EXTENT_PREALLOC: u8 = 2;

/// One resolved piece of a file's contents.
enum Piece<'a> {
    /// Data stored inside the item itself, already decoded.
    Inline(std::borrow::Cow<'a, [u8]>),
    /// Data on disk: logical address, and the length to read.
    Regular { logical: u64, len: u64 },
    /// A compressed run on disk that must be decoded whole, then sliced.
    ///
    /// The compressed unit is the entire extent, so unlike [`Piece::Regular`]
    /// the reference's `offset` cannot be folded into the address — it
    /// indexes the *decoded* bytes, and seeking by it on disk would land
    /// in the middle of a compressed stream. See [`crate::compression`].
    Compressed {
        /// Start of the compressed run.
        logical: u64,
        /// Its length on disk.
        disk_len: u64,
        /// What the whole run decodes to.
        ram_len: u64,
        /// Where this reference starts within the decoded bytes.
        offset: u64,
        /// How much of the decoded bytes this reference covers.
        len: u64,
        algo: Compression,
    },
    /// A hole or an unwritten preallocated extent.
    ///
    /// Carries no length: the output buffer is zeroed before any extent
    /// is copied into it, so a region with nothing to copy is already
    /// correct. Naming the case explicitly rather than falling through
    /// keeps the reason visible at the match site.
    Zeros,
}

/// One extent of a file, located both in the file and on the volume.
pub(crate) struct FileExtent {
    /// Offset within the file where this extent begins.
    pub start: u64,
    /// How much of the file it covers.
    pub len: u64,
    /// Where its bytes are, when they can be written in place at all.
    pub logical: Option<u64>,
    /// Start of the whole extent run, which is what the extent tree
    /// keys its reference count by.
    pub extent_start: u64,
    /// Whether the bytes on disk are compressed.
    pub compressed: bool,
}

/// A mounted Btrfs filesystem.
pub struct Filesystem {
    pub(crate) device: Arc<dyn BlockRead>,
    /// The devices of a pool, by the id chunk stripes reference.
    ///
    /// Empty for a single-device filesystem, where [`Self::device`] is
    /// everything and a mapping's `devid` cannot be anything else. When
    /// a filesystem spans several devices this holds all of them,
    /// including the one in [`Self::device`], because a stripe names
    /// the disk it is on and reading it from any other returns whatever
    /// happens to be at that offset.
    pub(crate) devices: BTreeMap<u64, Arc<dyn BlockRead>>,
    /// The same device again, present only when the volume was opened
    /// for writing. Kept separately so that "can this mount write" is a
    /// property of the type: the write path cannot compile without
    /// going through this field.
    pub(crate) writable: Option<Arc<dyn BlockDevice>>,
    pub(crate) sb: Superblock,
    pub(crate) map: ChunkMap,
    fs_tree_root: u64,
    /// Every item in the fs tree, keyed by its on-disk key.
    ///
    /// Loaded once at mount. Btrfs answers even a single `stat` by
    /// descending from the tree root, so a driver that re-descends per
    /// call re-reads the same interior nodes constantly. Holding the
    /// items costs memory proportional to the metadata rather than the
    /// data, which for a read-only driver is the right trade.
    items: BTreeMap<(u64, u8, u64), Vec<u8>>,
}

impl Filesystem {
    /// Open `device` as a Btrfs filesystem.
    pub fn mount(device: Arc<dyn BlockRead>) -> Result<Self> {
        Self::open(device, None)
    }

    /// Open `device` for reading **and writing**.
    ///
    /// Writing is opt-in rather than inferred from the device being
    /// writable: a driver able to write should not do so merely because
    /// nothing stopped it.
    ///
    /// The refusal of a non-empty log tree applies here as to a
    /// read-only mount, and matters more — writing to a volume whose log
    /// holds changes the trees have not seen would layer new data on top
    /// of state that is about to be replayed over it.
    pub fn mount_rw(device: Arc<dyn BlockDevice>) -> Result<Self> {
        if !device.is_writable() {
            return Err(Error::ReadOnly);
        }
        Self::open(device.clone(), Some(device))
    }

    /// Whether this mount can write.
    pub fn is_writable(&self) -> bool {
        self.writable.is_some()
    }

    /// Write to a logical address, following the chunk map exactly as
    /// the read path does — a write that ignored a chunk boundary would
    /// run past the end of one device and into another.
    /// Write to EVERY copy of a logical range.
    ///
    /// [`Filesystem::write_logical`] writes the first copy only, which is
    /// right for reading and wrong for writing. On a `DUP` or `RAID1`
    /// chunk it leaves the other copy holding what was there before, and
    /// the two then disagree with no record of which is current — a
    /// later read may return either.
    ///
    /// The commit trace confirms this is what the kernel does: both
    /// mirrors of every tree block go out BEFORE the barrier, so a torn
    /// write to one leaves the other and the barrier still orders both
    /// against the superblock.
    ///
    /// # Errors
    ///
    /// Propagates the first write failure. A partial result is possible
    /// and is not cleaned up: some mirrors may hold the new contents and
    /// some the old, which is the same state a power loss produces and
    /// is what the commit ordering exists to survive.
    pub(crate) fn write_logical_all_mirrors(
        device: &Arc<dyn BlockDevice>,
        map: &ChunkMap,
        logical: u64,
        buf: &[u8],
    ) -> Result<()> {
        let mirrors = map.mirrors_at(logical)?;
        for mirror in 0..mirrors {
            let mut done = 0usize;
            while done < buf.len() {
                let m = map.map_mirror(logical + done as u64, mirror)?;
                let n = (m.len as usize).min(buf.len() - done);
                if n == 0 {
                    return Err(Error::UnmappedLogical(logical + done as u64));
                }
                device.write_at(m.physical, &buf[done..done + n])?;
                done += n;
            }
        }
        Ok(())
    }

    pub(crate) fn write_logical(
        device: &Arc<dyn BlockDevice>,
        map: &ChunkMap,
        logical: u64,
        buf: &[u8],
    ) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let m = map.map(logical + done as u64)?;
            let n = (m.len as usize).min(buf.len() - done);
            if n == 0 {
                return Err(Error::UnmappedLogical(logical + done as u64));
            }
            device.write_at(m.physical, &buf[done..done + n])?;
            done += n;
        }
        Ok(())
    }

    fn open(device: Arc<dyn BlockRead>, writable: Option<Arc<dyn BlockDevice>>) -> Result<Self> {
        Self::open_pool(device, BTreeMap::new(), writable)
    }

    /// Open a filesystem that spans several devices.
    ///
    /// Every device of the pool must be given. A chunk stripe names the
    /// disk it lives on, so a missing device is not a partial view —
    /// reads of anything on it return whatever lies at that offset on
    /// whichever disk was consulted instead, which parses and then fails
    /// a checksum against a block it was never meant to be. On a
    /// mirrored pool it may not even fail.
    ///
    /// The devices are identified by the `devid` in each one's own
    /// superblock, not by the order they are passed in.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedFeature`] when the set is incomplete, when
    /// two of them are not the same filesystem, or when two claim the
    /// same device id.
    pub fn mount_pool(devices: Vec<Arc<dyn BlockRead>>) -> Result<Self> {
        if devices.is_empty() {
            return Err(Error::UnsupportedFeature(
                "a pool needs at least one device".to_string(),
            ));
        }

        // Each device's own superblock says which device it is and which
        // filesystem it belongs to.
        let mut by_id: BTreeMap<u64, Arc<dyn BlockRead>> = BTreeMap::new();
        let mut fsid: Option<[u8; 16]> = None;
        for dev in devices {
            let mut buf = vec![0u8; 4096];
            dev.read_at(SUPER_INFO_OFFSET, &mut buf)?;
            let sb = Superblock::parse_at(&buf, SUPER_INFO_OFFSET)?;

            match fsid {
                None => fsid = Some(sb.fsid),
                Some(seen) if seen != sb.fsid => {
                    return Err(Error::UnsupportedFeature(
                        "these devices belong to different filesystems".to_string(),
                    ))
                }
                Some(_) => {}
            }

            let id = sb.dev_item.devid;
            if by_id.insert(id, dev).is_some() {
                return Err(Error::UnsupportedFeature(format!(
                    "two devices both claim to be device {id}"
                )));
            }
        }

        // Read the filesystem through whichever device holds the
        // superblock's own copy; the rest are reached by devid.
        let first = by_id
            .values()
            .next()
            .expect("at least one device, checked above")
            .clone();
        Self::open_pool(first, by_id, None)
    }

    fn open_pool(
        device: Arc<dyn BlockRead>,
        devices: BTreeMap<u64, Arc<dyn BlockRead>>,
        writable: Option<Arc<dyn BlockDevice>>,
    ) -> Result<Self> {
        let mut sb_buf = vec![0u8; 4096];
        device.read_at(SUPER_INFO_OFFSET, &mut sb_buf)?;
        let sb = Superblock::parse_at(&sb_buf, SUPER_INFO_OFFSET)?;

        // One device open, and the filesystem says it has more.
        //
        // A chunk stripe names the device it lives on, and with only one
        // device there is nothing to do about a stripe naming another —
        // except refuse. Reading it from the device at hand returns
        // whatever is at that offset on THIS disk: it parses, it fails
        // its checksum against a block it was never meant to be, and on
        // a RAID1 filesystem where the mirror happens to hold the same
        // data it does not even fail. Silently reading one disk of a
        // pool as though it were the whole pool is worse than not
        // opening it.
        if sb.num_devices > 1 && devices.len() as u64 != sb.num_devices {
            return Err(Error::UnsupportedFeature(format!(
                "this filesystem spans {} devices and {} {} given; reading one of them \
                 alone would return the wrong data rather than fail. Open it with \
                 `mount_pool`, giving every device.",
                sb.num_devices,
                devices.len(),
                if devices.len() == 1 { "was" } else { "were" }
            )));
        }

        if sb.log_root != 0 {
            return Err(Error::DirtyLog);
        }

        // Step 2: the bootstrap map, enough to reach the chunk tree.
        let boot = ChunkMap::bootstrap(&sb)?;

        // Step 3: walk the chunk tree through it and fold in every chunk.
        let mut map = boot.clone();
        {
            let read = |logical: u64, buf: &mut [u8]| -> Result<()> {
                Self::read_logical(&device, &boot, logical, buf)
            };
            let tree = Tree::from_superblock(&sb, &read);
            let mut found = Vec::new();
            tree.for_each(sb.chunk_root, &mut |key: &DiskKey, data: &[u8]| {
                if let Ok(chunk) = Chunk::parse(key.offset, data) {
                    found.push(chunk);
                }
                Ok(true)
            })?;
            // The sys_chunk_array in the superblock is a copy of entries
            // that also live in the chunk tree, so folding the tree in
            // re-encounters them. An identical chunk is not a conflict —
            // skip it. A chunk that covers the same address with
            // DIFFERENT contents is a real inconsistency and must not be
            // silently discarded, so it still propagates.
            for chunk in found {
                match map.chunk_for(chunk.logical) {
                    Some(existing) if *existing == chunk => continue,
                    _ => map.insert(chunk)?,
                }
            }
        }

        // Step 4: the root tree names the fs tree.
        let fs_tree_root = {
            let read = |logical: u64, buf: &mut [u8]| -> Result<()> {
                Self::read_logical(&device, &map, logical, buf)
            };
            let tree = Tree::from_superblock(&sb, &read);
            let mut root = None;
            tree.for_each(sb.root, &mut |key: &DiskKey, data: &[u8]| {
                if key.objectid == FS_TREE_OBJECTID
                    && key.key_type == ROOT_ITEM_KEY
                    && data.len() > root_item::LEVEL
                {
                    root = Some(le64(data, root_item::BYTENR));
                }
                Ok(true)
            })?;
            root.ok_or_else(|| {
                Error::BadSuperblock("the root tree holds no ROOT_ITEM for the fs tree".into())
            })?
        };

        let mut fs = Filesystem {
            device,
            devices,
            writable,
            sb,
            map,
            fs_tree_root,
            items: BTreeMap::new(),
        };
        fs.load_fs_tree()?;
        Ok(fs)
    }

    /// Read `buf.len()` bytes at a logical address through `map`.
    pub(crate) fn read_logical(
        device: &Arc<dyn BlockRead>,
        map: &ChunkMap,
        logical: u64,
        buf: &mut [u8],
    ) -> Result<()> {
        Self::read_logical_on(device, map, u64::MAX, logical, buf)
    }

    /// Read a logical range from whichever device of a pool holds it.
    ///
    /// `devices` is empty for a single-device filesystem, and then this
    /// is [`Self::read_logical`]. Otherwise every mapping is answered by
    /// the device its `devid` names — which is the whole difference
    /// between reading a pool and reading one disk of it.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedFeature`] when a mapping names a device that
    /// was not given. That is not recoverable by trying another: the
    /// bytes are somewhere this filesystem cannot see.
    pub(crate) fn read_logical_pool(
        device: &Arc<dyn BlockRead>,
        devices: &BTreeMap<u64, Arc<dyn BlockRead>>,
        map: &ChunkMap,
        logical: u64,
        buf: &mut [u8],
    ) -> Result<()> {
        if devices.is_empty() {
            return Self::read_logical(device, map, logical, buf);
        }
        let mut done = 0usize;
        while done < buf.len() {
            let m = map.map(logical + done as u64)?;
            let n = (m.len as usize).min(buf.len() - done);
            if n == 0 {
                return Err(Error::UnmappedLogical(logical + done as u64));
            }
            let dev = devices.get(&m.devid).ok_or_else(|| {
                Error::UnsupportedFeature(format!(
                    "the range at {} lives on device {}, which was not given",
                    logical + done as u64,
                    m.devid
                ))
            })?;
            dev.read_at(m.physical, &mut buf[done..done + n])?;
            done += n;
        }
        Ok(())
    }

    /// Read a logical range, refusing anything that lives on a device
    /// other than `devid`.
    ///
    /// A [`Mapping`] names the device its physical offset is on, and
    /// with one device open there is nothing to do about a mapping that
    /// names another — except say so. Reading it from the device at hand
    /// returns whatever happens to be at that offset, which parses,
    /// checksums against the wrong block, and is silently the wrong
    /// data.
    ///
    /// `u64::MAX` means "do not check", used where the caller has
    /// already established there is only one device.
    pub(crate) fn read_logical_on(
        device: &Arc<dyn BlockRead>,
        map: &ChunkMap,
        devid: u64,
        logical: u64,
        buf: &mut [u8],
    ) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let m = map.map(logical + done as u64)?;
            if devid != u64::MAX && m.devid != devid {
                return Err(Error::UnsupportedFeature(format!(
                    "the range at {} lives on device {} and this filesystem was opened \
                     with device {devid} alone; reading it from the wrong device would \
                     return the wrong data rather than fail",
                    logical + done as u64,
                    m.devid
                )));
            }
            // A read may span two chunks, so never take more than the
            // mapping says is contiguous.
            let n = (m.len as usize).min(buf.len() - done);
            if n == 0 {
                return Err(Error::UnmappedLogical(logical + done as u64));
            }
            device.read_at(m.physical, &mut buf[done..done + n])?;
            done += n;
        }
        Ok(())
    }

    /// A handle over the same device, reading a different tree.
    ///
    /// Used by [`Filesystem::open_subvolume`]. The device, superblock
    /// and chunk map are shared — they describe the volume rather than
    /// any one tree — and only the root and the items loaded from it
    /// differ.
    ///
    /// The write capability is deliberately not carried across; see the
    /// note on `open_subvolume`.
    pub(crate) fn reroot(&self, fs_tree_root: u64) -> Result<Self> {
        let mut fs = Filesystem {
            device: self.device.clone(),
            devices: self.devices.clone(),
            writable: None,
            sb: self.sb.clone(),
            map: self.map.clone(),
            fs_tree_root,
            items: BTreeMap::new(),
        };
        fs.load_fs_tree()?;
        Ok(fs)
    }

    fn load_fs_tree(&mut self) -> Result<()> {
        let device = self.device.clone();
        let map = self.map.clone();
        let read = |logical: u64, buf: &mut [u8]| -> Result<()> {
            Self::read_logical(&device, &map, logical, buf)
        };
        let tree = Tree::new(TreeGeometry::from_superblock(&self.sb), &read);

        let mut items = BTreeMap::new();
        tree.for_each(self.fs_tree_root, &mut |key: &DiskKey, data: &[u8]| {
            items.insert((key.objectid, key.key_type, key.offset), data.to_vec());
            Ok(true)
        })?;
        self.items = items;
        Ok(())
    }

    /// The parsed superblock.
    /// Read one tree block by its logical address.
    ///
    /// A writer needs to look at a specific block rather than iterate
    /// items: to find what is above a leaf, to check what is at an
    /// address before placing something there.
    ///
    /// # Errors
    ///
    /// Propagates the read, and the block's own verification — an
    /// address holding something that is not a tree block of this
    /// filesystem is an error rather than an empty result.
    pub fn read_tree_block(&self, logical: u64) -> Result<crate::btree::TreeBlock> {
        let read = |logical: u64, buf: &mut [u8]| -> Result<()> {
            Self::read_logical_pool(&self.device, &self.devices, &self.map, logical, buf)
        };
        crate::btree::Tree::from_superblock(&self.sb, &read).read_block(logical)
    }

    /// The chunk map — how logical addresses become physical ones.
    ///
    /// Exposed because a WRITER has to reason about placement in a way a
    /// reader does not: how many copies an address has, and where each
    /// one lands.
    pub fn chunk_map(&self) -> &ChunkMap {
        &self.map
    }

    pub fn superblock(&self) -> &Superblock {
        &self.sb
    }

    /// Every item in the root tree.
    ///
    /// The root tree is the index of trees: one `ROOT_ITEM` per
    /// subvolume saying where its tree lives, and reference items
    /// naming them. This hands the raw items back so a caller can see
    /// what is actually there rather than only what is understood.
    ///
    /// # Errors
    ///
    /// As the B-tree walk.
    pub fn root_tree_items(&self) -> Result<Vec<RootTreeItem>> {
        let read = |logical: u64, buf: &mut [u8]| -> Result<()> {
            Self::read_logical_pool(&self.device, &self.devices, &self.map, logical, buf)
        };
        let tree = Tree::from_superblock(&self.sb, &read);
        let mut out = Vec::new();
        tree.for_each(self.sb.root, &mut |key: &DiskKey, data: &[u8]| {
            out.push((key.objectid, key.key_type, key.offset, data.to_vec()));
            Ok(true)
        })?;
        Ok(out)
    }

    /// Read one inode by objectid.
    pub fn read_inode(&self, ino: u64) -> Result<Inode> {
        let data = self
            .items
            .get(&(ino, INODE_ITEM_KEY, 0))
            .ok_or(Error::NotFound)?;
        Inode::parse(data, ino)
    }

    /// The root directory's inode.
    pub fn root_inode(&self) -> Result<Inode> {
        self.read_inode(FIRST_FREE_OBJECTID)
    }

    /// List a directory's entries.
    ///
    /// Uses `DIR_INDEX` rather than `DIR_ITEM`: the index is ordered and
    /// holds exactly one entry per key, while `DIR_ITEM` is hashed and
    /// packs colliding names into a single value. Both describe the same
    /// set; the index is simply the one meant for iteration.
    ///
    /// `.` and `..` are never returned, matching the sibling XFS driver
    /// so a caller does not have to special-case per filesystem.
    pub fn read_dir(&self, ino: u64) -> Result<Vec<DirEntry>> {
        let inode = self.read_inode(ino)?;
        if !inode.is_dir() {
            return Err(Error::NotADirectory);
        }
        let mut out = Vec::new();
        for ((objectid, key_type, _), data) in self
            .items
            .range((ino, DIR_INDEX_KEY, 0)..=(ino, DIR_INDEX_KEY, u64::MAX))
        {
            if *objectid != ino || *key_type != DIR_INDEX_KEY {
                break;
            }
            for e in dir::parse_dir_items(data)? {
                if e.name != b"." && e.name != b".." {
                    out.push(e);
                }
            }
        }
        Ok(out)
    }

    /// Look up one name within a directory.
    pub fn lookup(&self, dir_ino: u64, name: &[u8]) -> Result<Inode> {
        let hit = self
            .read_dir(dir_ino)?
            .into_iter()
            .find(|e| e.name == name)
            .ok_or(Error::NotFound)?;

        // A subvolume is a directory entry whose location names a tree
        // rather than an inode, so there is nothing in THIS tree to
        // return. Saying so is the point: reading the entry's objectid
        // as an inode number finds an unrelated inode of the same
        // number, or nothing, and `NotFound` for a name that is plainly
        // there sends the reader looking in the wrong place entirely.
        if !hit.is_inode() {
            return Err(Error::UnsupportedFeature(format!(
                "{:?} names subvolume {} rather than an inode in this tree — open it \
                 with `open_subvolume({})` and look the rest of the path up in there",
                String::from_utf8_lossy(name),
                hit.ino,
                hit.ino
            )));
        }
        self.read_inode(hit.ino)
    }

    /// Resolve an absolute path to its inode.
    ///
    /// Symbolic links are not followed, so link loops remain the
    /// caller's policy rather than a surprise from this function.
    pub fn lookup_path(&self, path: &str) -> Result<Inode> {
        let mut inode = self.root_inode()?;
        for component in path.split('/').filter(|c| !c.is_empty() && *c != ".") {
            if component == ".." {
                return Err(Error::UnsupportedFeature(
                    "`..` in a path is not resolved by lookup_path".into(),
                ));
            }
            if !inode.is_dir() {
                return Err(Error::NotADirectory);
            }
            inode = self.lookup(inode.ino, component.as_bytes())?;
        }
        Ok(inode)
    }

    /// Decode one `EXTENT_DATA` item into the piece of file it describes.
    fn decode_extent<'a>(&self, data: &'a [u8], ino: u64) -> Result<Piece<'a>> {
        if data.len() < file_extent::TYPE + 1 {
            return Err(Error::BadSuperblock(format!(
                "inode {ino}: extent item is {} bytes, too short to hold a type",
                data.len()
            )));
        }
        let _generation = le64(data, file_extent::GENERATION);
        let ram_bytes = le64(data, file_extent::RAM_BYTES);
        let compression = data[file_extent::COMPRESSION];
        let encryption = data[file_extent::ENCRYPTION];
        let other = u16::from_le_bytes(
            data[file_extent::OTHER_ENCODING..file_extent::OTHER_ENCODING + 2]
                .try_into()
                .expect("2 bytes"),
        );
        let kind = data[file_extent::TYPE];

        let algo = Compression::from_byte(compression)
            .map_err(|e| Error::UnsupportedFeature(format!("inode {ino}: {e}")))?;
        if encryption != 0 || other != 0 {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino}: extent is encoded (encryption {encryption}, other {other})"
            )));
        }

        match kind {
            EXTENT_INLINE => {
                let end = data.len();
                let start = file_extent::INLINE_DATA.min(end);
                let raw = &data[start..end];
                // An inline extent may be compressed too, and there is no
                // offset to apply: the item holds the whole thing.
                Ok(Piece::Inline(if algo.is_compressed() {
                    std::borrow::Cow::Owned(compression::decompress(
                        algo,
                        raw,
                        ram_bytes as usize,
                        self.sb.sectorsize as usize,
                    )?)
                } else {
                    std::borrow::Cow::Borrowed(raw)
                }))
            }
            EXTENT_REGULAR | EXTENT_PREALLOC => {
                if data.len() < file_extent::REGULAR_SIZE {
                    return Err(Error::BadSuperblock(format!(
                        "inode {ino}: non-inline extent item is {} bytes, need {}",
                        data.len(),
                        file_extent::REGULAR_SIZE
                    )));
                }
                let disk_bytenr = le64(data, file_extent::DISK_BYTENR);
                let offset = le64(data, file_extent::OFFSET);
                let num_bytes = le64(data, file_extent::NUM_BYTES);

                // disk_bytenr == 0 is a hole. A preallocated extent has
                // blocks reserved but never written, and returning them
                // would disclose whatever previously occupied the space.
                if disk_bytenr == 0 || kind == EXTENT_PREALLOC {
                    let _ = num_bytes;
                    return Ok(Piece::Zeros);
                }
                if algo.is_compressed() {
                    return Ok(Piece::Compressed {
                        logical: disk_bytenr,
                        disk_len: le64(data, file_extent::DISK_NUM_BYTES),
                        ram_len: ram_bytes,
                        offset,
                        len: num_bytes,
                        algo,
                    });
                }
                Ok(Piece::Regular {
                    logical: disk_bytenr + offset,
                    len: num_bytes,
                })
            }
            other => Err(Error::BadSuperblock(format!(
                "inode {ino}: extent type {other} is not a defined value (ram_bytes {ram_bytes})"
            ))),
        }
    }

    /// One extent of a file, as the write planner needs to see it.
    ///
    /// The read path consumes `Piece` and copies immediately; a writer
    /// has to decide whether a range may be written at all before
    /// touching any of it, which needs the pieces as a list with their
    /// file offsets attached.
    pub(crate) fn file_extents(&self, ino: u64) -> Result<Vec<FileExtent>> {
        let mut out = Vec::new();
        for ((objectid, key_type, offset), data) in self
            .items
            .range((ino, EXTENT_DATA_KEY, 0)..=(ino, EXTENT_DATA_KEY, u64::MAX))
        {
            if *objectid != ino || *key_type != EXTENT_DATA_KEY {
                break;
            }
            let start = *offset;
            match self.decode_extent(data, ino)? {
                // Inline data lives in the item, so there is no block to
                // overwrite; preallocated and holes have nothing behind
                // them. All three are reported with no logical address,
                // and the planner refuses them by name.
                Piece::Inline(bytes) => out.push(FileExtent {
                    start,
                    len: bytes.len() as u64,
                    logical: None,
                    extent_start: 0,
                    compressed: false,
                }),
                Piece::Zeros => {}
                Piece::Regular { logical, len } => out.push(FileExtent {
                    start,
                    len,
                    logical: Some(logical),
                    // `logical` already has the reference's offset folded
                    // in; the extent item is keyed by the run's start.
                    extent_start: le64(data, file_extent::DISK_BYTENR),
                    compressed: false,
                }),
                Piece::Compressed { len, .. } => out.push(FileExtent {
                    start,
                    len,
                    logical: None,
                    extent_start: le64(data, file_extent::DISK_BYTENR),
                    compressed: true,
                }),
            }
        }
        Ok(out)
    }

    /// Read a whole file.
    pub fn read_file(&self, ino: u64) -> Result<Vec<u8>> {
        let inode = self.read_inode(ino)?;
        if !inode.is_regular_file() && !inode.is_symlink() {
            return Err(Error::NotAFile);
        }
        let size = inode.size as usize;
        // Start from zeros so holes and preallocated extents need no
        // special case on the copy path.
        let mut out = vec![0u8; size];

        for ((objectid, key_type, offset), data) in self
            .items
            .range((ino, EXTENT_DATA_KEY, 0)..=(ino, EXTENT_DATA_KEY, u64::MAX))
        {
            if *objectid != ino || *key_type != EXTENT_DATA_KEY {
                break;
            }
            let at = *offset as usize;
            if at >= size {
                continue;
            }
            match self.decode_extent(data, ino)? {
                Piece::Inline(bytes) => {
                    let n = bytes.len().min(size - at);
                    out[at..at + n].copy_from_slice(&bytes[..n]);
                }
                Piece::Zeros => {}
                Piece::Regular { logical, len } => {
                    let n = (len as usize).min(size - at);
                    if n > 0 {
                        Self::read_logical_pool(
                            &self.device,
                            &self.devices,
                            &self.map,
                            logical,
                            &mut out[at..at + n],
                        )?;
                    }
                }
                Piece::Compressed {
                    logical,
                    disk_len,
                    ram_len,
                    offset: within,
                    len,
                    algo,
                } => {
                    let mut packed = vec![0u8; disk_len as usize];
                    Self::read_logical_pool(
                        &self.device,
                        &self.devices,
                        &self.map,
                        logical,
                        &mut packed,
                    )?;
                    let decoded = compression::decompress(
                        algo,
                        &packed,
                        ram_len as usize,
                        self.sb.sectorsize as usize,
                    )?;
                    // `within` indexes the decoded bytes, which is the
                    // whole reason this is not a Regular read.
                    let from = (within as usize).min(decoded.len());
                    let take = (len as usize).min(decoded.len() - from).min(size - at);
                    out[at..at + take].copy_from_slice(&decoded[from..from + take]);
                }
            }
        }
        Ok(out)
    }

    /// Read part of a file.
    ///
    /// Returns the number of bytes read, short only at end of file.
    pub fn read_at(&self, ino: u64, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let inode = self.read_inode(ino)?;
        if offset >= inode.size {
            return Ok(0);
        }
        let whole = self.read_file(ino)?;
        let start = offset as usize;
        let n = buf.len().min(whole.len().saturating_sub(start));
        buf[..n].copy_from_slice(&whole[start..start + n]);
        Ok(n)
    }

    /// Resolve a symbolic link's target.
    pub fn read_link(&self, ino: u64) -> Result<Vec<u8>> {
        let inode = self.read_inode(ino)?;
        if !inode.is_symlink() {
            return Err(Error::NotAFile);
        }
        self.read_file(ino)
    }

    /// List a directory by path.
    pub fn list_path(&self, path: &str) -> Result<Vec<DirEntry>> {
        let inode = self.lookup_path(path)?;
        self.read_dir(inode.ino)
    }

    /// Read a whole file by path.
    pub fn read_path(&self, path: &str) -> Result<Vec<u8>> {
        let inode = self.lookup_path(path)?;
        self.read_file(inode.ino)
    }
}
