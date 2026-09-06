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

/// The root block address the root tree records for `objectid`.
///
/// This existed three times — `block_group::tree_root`,
/// `write::extent_tree_root`, and inline in `fs::open_pool` for the FS
/// tree — and **the three did not agree**, in two ways that no test
/// distinguishes because a real `ROOT_ITEM` is 439 bytes and none of
/// the disagreements can trigger on one:
///
/// * two required `data.len() > root_item::LEVEL` (238) and one
///   required `data.len() > root_item::BYTENR + 8` (184), so a
///   truncated item of 185..=238 bytes was accepted by one and rejected
///   by the others;
/// * two stopped at the first matching `ROOT_ITEM` and one kept
///   scanning and took the **last**, so a root tree holding two items
///   for the same objectid would have been read differently depending
///   on which caller asked.
///
/// This settles both. The bound is `BYTENR + 8` — the bytes actually
/// read — because a bound of `LEVEL` refuses items this function could
/// answer from. And it takes the **first** match and stops, which is
/// what a tree with one `ROOT_ITEM` per objectid means; if there are
/// two, the tree is already malformed and reading further does not make
/// the answer better.
///
/// `tree` walks the root tree, so callers differ only in which read
/// closure they built it from — which is the only thing that ever
/// genuinely differed between the three copies.
///
/// # Errors
///
/// [`Error::BadSuperblock`] when there is no `ROOT_ITEM` for
/// `objectid`. For an optional tree, that is how a caller learns the
/// tree is absent.
pub(crate) fn root_item_target(
    tree: &crate::btree::Tree,
    root_tree: u64,
    objectid: u64,
) -> Result<u64> {
    let mut root = None;
    tree.for_each(root_tree, &mut |key: &DiskKey, data: &[u8]| {
        if key.objectid == objectid
            && key.key_type == ROOT_ITEM_KEY
            && data.len() >= root_item::BYTENR + 8
        {
            root = Some(le64(data, root_item::BYTENR));
            return Ok(false);
        }
        Ok(true)
    })?;
    root.ok_or_else(|| {
        Error::BadSuperblock(format!(
            "the root tree holds no ROOT_ITEM for tree {objectid}"
        ))
    })
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

/// The longest a symbolic link's target can be.
///
/// `PATH_MAX`. A link's target is a path and the operating system will
/// not take a longer one, while the inode's declared size is a raw
/// `le64`.
pub const MAX_SYMLINK_TARGET: u64 = 4096;

/// An extent that does not hold the bytes it claims to cover.
fn short_extent(ino: u64, kind: &str) -> Error {
    Error::BadSuperblock(format!(
        "inode {ino}: a {kind} extent is shorter than the range it covers"
    ))
}

/// How many blocks a mount caches by default: **none**.
///
/// Not an oversight, and not a placeholder. Measured — see
/// `docs/read-path-cost.md` — a block cache buys this driver nothing
/// and costs it something:
///
/// - Every item of the fs tree is loaded at mount, so a walk, a stat
///   and a read make **zero** calls to the device afterwards. There are
///   no repeat metadata reads left for a cache to serve.
/// - Metadata is read a node at a time, and a node is `nodesize` —
///   typically four sectors. Caching by sector turns one call into
///   four, so on the `rich` fixture the cached mount asked the device
///   for 14 reads where the uncached one asked for 4.
///
/// [`Filesystem::mount_with_cache`] still exists so the measurement can
/// take both passes, and so the decision can be re-taken against a
/// number if the eager load is ever replaced by lazy descent — at which
/// point a cache becomes worth having and this constant should change
/// with it.
pub const DEFAULT_CACHE_BLOCKS: usize = 0;

impl Filesystem {
    /// Open `device` as a Btrfs filesystem.
    pub fn mount(device: Arc<dyn BlockRead>) -> Result<Self> {
        Self::mount_with_cache(device, DEFAULT_CACHE_BLOCKS)
    }

    /// Open `device` for reading, caching `blocks` metadata blocks.
    ///
    /// # Why the cache is built here and not by the caller
    ///
    /// It is sized in blocks, and the block size is the filesystem's.
    /// A caller wanting to wrap the device itself would have to parse a
    /// superblock first to know what to wrap it with — which is what
    /// this does, once, before wrapping.
    ///
    /// # What it is for
    ///
    /// Every lookup descends from the root of the filesystem tree, so
    /// the nodes near that root are read again for each path, and the
    /// chunk map is consulted for every logical address translated.
    /// None of those bytes change during a mount.
    ///
    /// `blocks` of zero disables it, which is what the measurement in
    /// `tests/read_path_cost.rs` uses to take its baseline.
    pub fn mount_with_cache(device: Arc<dyn BlockRead>, blocks: usize) -> Result<Self> {
        if blocks == 0 {
            return Self::open(device, None);
        }
        // The sector size is not known until a superblock has been
        // parsed, and the superblock is at a fixed offset, so this one
        // read goes to the device directly.
        let mut sb_buf = vec![0u8; 4096];
        device.read_at(SUPER_INFO_OFFSET, &mut sb_buf)?;
        let sb = Superblock::parse_at(&sb_buf, SUPER_INFO_OFFSET)?;

        // CACHED BY SECTOR RATHER THAN BY NODE. A node is `nodesize`,
        // typically 16 KiB, and caching whole nodes would make the unit
        // four times larger than the smallest useful read. Sectors are
        // the finer unit and a node is then four cached blocks, stitched
        // by the cache itself.
        let device: Arc<dyn BlockRead> =
            fs_core::CachingDevice::read_only(device, u64::from(sb.sectorsize), blocks);
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
    /// Where a mapped range lands on the device, once it is known to be
    /// on it.
    ///
    /// A chunk's `btrfs_stripe.offset` is a raw `u64` off the disk, and
    /// `Chunk::validate_geometry` checks it for sector alignment and
    /// nothing else -- not against `dev_item.total_bytes`, not against
    /// the device. So every tree block and every data byte this crate
    /// writes could land at an offset the image chose. Against a
    /// file-backed image that is not an error at all: `write_at` on a
    /// file extends it, so a chunk at `stripe.offset = 2^60` grows a
    /// mounted image toward an exabyte. On a `CallbackDevice` the
    /// offset goes straight to the host.
    ///
    /// `commit::superblock_copy_fits` applies exactly this rule to the
    /// superblock copies; nothing else did.
    fn writable_span(device: &Arc<dyn BlockDevice>, physical: u64, len: usize) -> Result<()> {
        let end = physical.checked_add(len as u64).ok_or_else(|| {
            Error::BadSuperblock(format!(
                "a chunk maps a write to {physical}, which ends past the address space"
            ))
        })?;
        let device_bytes = device.size_bytes();
        if end > device_bytes {
            return Err(Error::BadSuperblock(format!(
                "a chunk maps a write to [{physical}, {end}) on a device of {device_bytes} bytes"
            )));
        }
        Ok(())
    }

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
                Self::writable_span(device, m.physical, n)?;
                Self::writable_span(device, m.physical, n)?;
                Self::writable_span(device, m.physical, n)?;
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
            root_item_target(&tree, sb.root, FS_TREE_OBJECTID)?
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
    /// A tree walker over this filesystem's pool.
    ///
    /// The four lines this replaces —
    ///
    /// ```ignore
    /// let read = |logical: u64, buf: &mut [u8]| -> Result<()> {
    ///     Self::read_logical_pool(&self.device, &self.devices, &self.map, logical, buf)
    /// };
    /// let tree = Tree::from_superblock(&self.sb, &read);
    /// ```
    ///
    /// appeared eleven times, character-identical in ten of them. The
    /// borrow checker is why they were never factored: the closure
    /// borrows three fields and the `Tree` borrows the closure, so a
    /// `fn tree(&self) -> Tree<'_>` cannot return both. Returning the
    /// *reader* instead, and taking the `Tree` from it, splits the two
    /// borrows across two statements, which is all that was needed.
    ///
    /// Callers write:
    ///
    /// ```ignore
    /// let reader = self.pool_reader();
    /// let tree = reader.tree();
    /// ```
    pub(crate) fn pool_reader(&self) -> PoolReader<'_> {
        PoolReader {
            geom: crate::btree::TreeGeometry::from_superblock(&self.sb),
            read: Box::new(move |logical, buf| {
                Self::read_logical_pool(&self.device, &self.devices, &self.map, logical, buf)
            }),
        }
    }

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
        let reader = self.pool_reader();
        reader.tree().read_block(logical)
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
        let reader = self.pool_reader();
        let tree = reader.tree();
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
                    // `disk_len` is the buffer the compressed bytes are
                    // read into, and it is a raw le64. Btrfs never
                    // writes a compressed extent larger than the unit it
                    // compresses in -- see `compression::MAX_COMPRESSED`.
                    let disk_len = le64(data, file_extent::DISK_NUM_BYTES);
                    if disk_len > compression::MAX_COMPRESSED {
                        return Err(Error::BadSuperblock(format!(
                            "inode {ino}: compressed extent occupies {disk_len} bytes on \
                             disk, more than the {} a compressed extent can",
                            compression::MAX_COMPRESSED
                        )));
                    }
                    return Ok(Piece::Compressed {
                        logical: disk_bytenr,
                        disk_len,
                        ram_len: ram_bytes,
                        offset,
                        len: num_bytes,
                        algo,
                    });
                }
                // THE ITEM'S WINDOW IS INSIDE THE EXTENT IT NAMES.
                //
                // `offset` says where in the extent this item's data
                // starts and `num_bytes` how much of it the item
                // covers, so together they cannot exceed the extent's
                // own length. The kernel's tree checker enforces
                // exactly this. Without it, one `u64` moved the write
                // target outside the extent entirely -- and the
                // reference check on the write path is keyed on
                // `disk_bytenr`, so it still found the extent, agreed
                // it had one owner, and let the write land somewhere
                // else: over a tree block, or over another file.
                let window_end = offset
                    .checked_add(num_bytes)
                    .filter(|end| *end <= ram_bytes);
                if window_end.is_none() {
                    return Err(Error::BadSuperblock(format!(
                        "inode {ino}: an extent item covers [{offset}, +{num_bytes}) of an \
                         extent that is {ram_bytes} bytes long"
                    )));
                }
                Ok(Piece::Regular {
                    // Both halves are raw le64s. In release, where this
                    // crate ships with overflow-checks off, the sum
                    // wrapped to a small logical address that then
                    // mapped into an unrelated chunk -- a silent
                    // mistranslation, which is the failure this driver
                    // exists to avoid.
                    logical: disk_bytenr.checked_add(offset).ok_or_else(|| {
                        Error::BadSuperblock(format!(
                            "inode {ino}: extent at {disk_bytenr} plus offset {offset} \
                             leaves the address space"
                        ))
                    })?,
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
    ///
    /// Materialises the file in memory, so it is bounded by the size of
    /// the filesystem: a whole-file read cannot need more memory than
    /// the filesystem has bytes, and `inode.size` is a raw `le64` that
    /// said otherwise. Reading part of a larger file is what
    /// [`Filesystem::read_at`] is for.
    pub fn read_file(&self, ino: u64) -> Result<Vec<u8>> {
        let inode = self.read_inode(ino)?;
        if !inode.is_regular_file() && !inode.is_symlink() {
            return Err(Error::NotAFile);
        }
        if inode.size > self.sb.total_bytes {
            return Err(Error::BadSuperblock(format!(
                "inode {ino} says it is {} bytes, more than the {} the filesystem holds",
                inode.size, self.sb.total_bytes
            )));
        }
        let mut out = vec![0u8; inode.size as usize];
        self.read_range(&inode, 0, &mut out)?;
        Ok(out)
    }

    /// Fill `buf` with the file's bytes from `from` onwards.
    ///
    /// `buf` is zeroed first, so a hole and a preallocated extent need
    /// no special case: they are the absence of a copy.
    ///
    /// Only the extents that overlap the window are read. `read_at`
    /// used to call `read_file` and slice the result, which meant
    /// serving a 4 KiB read of a large file by materialising the whole
    /// of it -- and `inode.size` is a raw `le64`, so "the whole of it"
    /// was whatever the image claimed.
    fn read_range(&self, inode: &Inode, from: u64, buf: &mut [u8]) -> Result<()> {
        buf.fill(0);
        if buf.is_empty() {
            return Ok(());
        }
        let ino = inode.ino;
        let want_end = from.saturating_add(buf.len() as u64).min(inode.size);

        for ((objectid, key_type, offset), data) in self
            .items
            .range((ino, EXTENT_DATA_KEY, 0)..=(ino, EXTENT_DATA_KEY, u64::MAX))
        {
            if *objectid != ino || *key_type != EXTENT_DATA_KEY {
                break;
            }
            let at = *offset;
            if at >= want_end {
                break;
            }
            let piece = self.decode_extent(data, ino)?;
            let extent_len = match &piece {
                Piece::Inline(bytes) => bytes.len() as u64,
                Piece::Zeros => continue,
                Piece::Regular { len, .. } | Piece::Compressed { len, .. } => *len,
            };
            // Where this extent and the window overlap, in file offsets.
            let start = at.max(from);
            let end = at.saturating_add(extent_len).min(want_end);
            if end <= start {
                continue;
            }
            let take = (end - start) as usize;
            let skip = (start - at) as usize;
            let dst = &mut buf[(start - from) as usize..(end - from) as usize];

            match piece {
                Piece::Inline(bytes) => {
                    let src = bytes
                        .get(skip..skip + take)
                        .ok_or_else(|| short_extent(ino, "inline"))?;
                    dst.copy_from_slice(src);
                }
                Piece::Zeros => unreachable!("handled above"),
                Piece::Regular { logical, .. } => {
                    let at_logical = logical
                        .checked_add(skip as u64)
                        .ok_or_else(|| short_extent(ino, "regular"))?;
                    Self::read_logical_pool(
                        &self.device,
                        &self.devices,
                        &self.map,
                        at_logical,
                        dst,
                    )?;
                }
                Piece::Compressed {
                    logical,
                    disk_len,
                    ram_len,
                    offset: within,
                    algo,
                    ..
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
                    let src_at = (within as usize).saturating_add(skip);
                    let src = decoded
                        .get(src_at..src_at + take)
                        .ok_or_else(|| short_extent(ino, "compressed"))?;
                    dst.copy_from_slice(src);
                }
            }
        }
        Ok(())
    }

    /// Read part of a file.
    ///
    /// Returns the number of bytes read, short only at end of file.
    pub fn read_at(&self, ino: u64, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let inode = self.read_inode(ino)?;
        // Kept from when this went through `read_file`: reading a
        // directory is EISDIR, not a short read of nothing.
        if !inode.is_regular_file() && !inode.is_symlink() {
            return Err(Error::NotAFile);
        }
        if offset >= inode.size {
            return Ok(0);
        }
        let n = buf.len().min((inode.size - offset) as usize);
        self.read_range(&inode, offset, &mut buf[..n])?;
        Ok(n)
    }

    /// Resolve a symbolic link's target.
    pub fn read_link(&self, ino: u64) -> Result<Vec<u8>> {
        let inode = self.read_inode(ino)?;
        if !inode.is_symlink() {
            return Err(Error::NotAFile);
        }
        // A link's target is a path, and no path is longer than
        // PATH_MAX. Without this the inode's raw `size` reached
        // `read_file`'s allocation from `fs_btrfs_readlink`.
        if inode.size > MAX_SYMLINK_TARGET {
            return Err(Error::BadSuperblock(format!(
                "inode {ino}: symlink target of {} bytes is longer than any path",
                inode.size
            )));
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

/// A tree walker bound to one filesystem's pool.
///
/// Holds the read closure so a [`crate::btree::Tree`] can borrow it —
/// see [`Filesystem::pool_reader`] for why the two cannot be one value.
pub(crate) struct PoolReader<'a> {
    geom: crate::btree::TreeGeometry,
    read: OwnedReadBlock<'a>,
}

/// An owned block reader, the counterpart to
/// [`crate::btree::ReadBlock`]'s borrowed one.
///
/// The `Tree` borrows a reader; something has to own it, and that
/// something is [`PoolReader`].
type OwnedReadBlock<'a> = Box<dyn Fn(u64, &mut [u8]) -> Result<()> + 'a>;

impl PoolReader<'_> {
    /// A walker over any tree in this pool. The root address is a
    /// per-call argument, so one reader serves every tree.
    pub(crate) fn tree(&self) -> crate::btree::Tree<'_> {
        crate::btree::Tree::new(self.geom, &*self.read)
    }
}

#[cfg(test)]
mod root_item_target_tests {
    use super::*;
    use crate::btree::test_blocks::{geom, key, leaf, LEAF_A, NODESIZE};
    use crate::btree::Tree;

    /// A `ROOT_ITEM` body of exactly the bytes this function reads.
    ///
    /// A real one is 439 bytes; this is the shortest body from which the
    /// answer is still derivable. Two of the three copies this replaced
    /// required `len > root_item::LEVEL` (238) and would have refused
    /// it — which is the disagreement the consolidation had to settle,
    /// and settling it silently is what this test prevents.
    fn minimal_root_item(bytenr: u64) -> Vec<u8> {
        let mut body = vec![0u8; root_item::BYTENR + 8];
        body[root_item::BYTENR..root_item::BYTENR + 8].copy_from_slice(&bytenr.to_le_bytes());
        body
    }

    fn tree_over(block: Vec<u8>) -> (Vec<u8>, u64) {
        (block, LEAF_A)
    }

    fn lookup(entries: &[(DiskKey, Vec<u8>)], objectid: u64) -> Result<u64> {
        let (block, at) = tree_over(leaf(LEAF_A, crate::chunk::objectid::ROOT_TREE, entries));
        let read = |logical: u64, buf: &mut [u8]| -> Result<()> {
            assert_eq!(logical, at, "the walk asked for a block that is not there");
            buf.copy_from_slice(&block[..buf.len()]);
            Ok(())
        };
        let tree = Tree::new(geom(), &read);
        root_item_target(&tree, at, objectid)
    }

    /// The bound is what the function reads, not the whole structure.
    #[test]
    fn a_root_item_long_enough_to_answer_from_is_accepted() {
        let entries = vec![(key(7, ROOT_ITEM_KEY, 0), minimal_root_item(0xABCD_0000))];
        assert_eq!(lookup(&entries, 7).unwrap(), 0xABCD_0000);
    }

    /// One byte short and it is refused, rather than read past.
    #[test]
    fn a_root_item_one_byte_too_short_is_not_used() {
        let mut short = minimal_root_item(0xABCD_0000);
        short.pop();
        let entries = vec![(key(7, ROOT_ITEM_KEY, 0), short)];
        assert!(
            lookup(&entries, 7).is_err(),
            "a body that cannot hold the field must not be read from"
        );
    }

    /// Two `ROOT_ITEM`s for one objectid: the first wins.
    ///
    /// One of the three copies kept scanning and took the last. A tree
    /// with two items for the same objectid is already malformed, so
    /// neither answer is more correct — but they must not differ by
    /// which caller asked, and before this they did.
    #[test]
    fn the_first_matching_root_item_is_the_answer() {
        let entries = vec![
            (key(7, ROOT_ITEM_KEY, 0), minimal_root_item(0x1111_0000)),
            (key(7, ROOT_ITEM_KEY, 1), minimal_root_item(0x2222_0000)),
        ];
        assert_eq!(lookup(&entries, 7).unwrap(), 0x1111_0000);
    }

    /// An absent tree is an error, which is how an optional tree's
    /// caller learns it is not there.
    #[test]
    fn a_missing_root_item_is_an_error_naming_the_tree() {
        let entries = vec![(key(7, ROOT_ITEM_KEY, 0), minimal_root_item(1))];
        let err = lookup(&entries, 9).unwrap_err();
        assert!(
            err.to_string().contains('9'),
            "the error should name the tree that is missing, got: {err}"
        );
    }

    /// An item of the right objectid but the wrong type is not a match.
    #[test]
    fn only_root_items_are_considered() {
        let entries = vec![(key(7, ROOT_ITEM_KEY + 1, 0), minimal_root_item(0x3333_0000))];
        assert!(lookup(&entries, 7).is_err());
    }

    /// The block builder's own assumption, so a change to `NODESIZE`
    /// that makes these fixtures impossible fails here rather than
    /// somewhere confusing.
    #[test]
    fn the_fixture_leaf_has_room_for_two_root_items() {
        assert!(NODESIZE as usize > 2 * (root_item::BYTENR + 8) + 128);
    }
}
