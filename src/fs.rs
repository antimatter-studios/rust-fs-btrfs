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
//! - **Compressed extents.** Decompression is not implemented. Returning
//!   the raw compressed bytes would look like a successful read of
//!   corrupt data.
//! - **Encoded extents** — encrypted or otherwise transformed.
//! - **A dirty log.** `log_root` being set means the tree on disk is not
//!   the whole story.
//!
//! Holes read as zeros, which is what they are.

use crate::btree::{Tree, TreeGeometry};
use crate::chunk::{Chunk, ChunkMap, DiskKey};
use crate::dir::{self, DirEntry, DIR_INDEX_KEY};
use crate::error::{Error, Result};
use crate::inode::{Inode, FIRST_FREE_OBJECTID, INODE_ITEM_KEY};
use crate::superblock::{Superblock, SUPER_INFO_OFFSET};
use fs_core::BlockRead;
use std::collections::BTreeMap;
use std::sync::Arc;

/// `BTRFS_FS_TREE_OBJECTID` — the subvolume holding the default
/// filesystem namespace.
pub const FS_TREE_OBJECTID: u64 = 5;

/// `BTRFS_ROOT_ITEM_KEY`.
pub const ROOT_ITEM_KEY: u8 = 132;

/// `BTRFS_EXTENT_DATA_KEY` — a file's data, inline or by reference.
pub const EXTENT_DATA_KEY: u8 = 108;

/// Byte offsets within `struct btrfs_root_item`.
///
/// The item opens with an embedded `btrfs_inode_item` of 160 bytes,
/// followed by `generation` and `root_dirid` before the root address.
/// Corroborated against real media: a wrong value here yields an address
/// whose tree block fails its own identity check rather than producing
/// plausible garbage.
mod root_item {
    pub const BYTENR: usize = 176;
    pub const LEVEL: usize = 238;
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

#[inline]
fn le64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().expect("8 bytes"))
}

/// One resolved piece of a file's contents.
enum Piece<'a> {
    /// Data stored inside the item itself.
    Inline(&'a [u8]),
    /// Data on disk: logical address, and the length to read.
    Regular { logical: u64, len: u64 },
    /// A hole or an unwritten preallocated extent.
    ///
    /// Carries no length: the output buffer is zeroed before any extent
    /// is copied into it, so a region with nothing to copy is already
    /// correct. Naming the case explicitly rather than falling through
    /// keeps the reason visible at the match site.
    Zeros,
}

/// A mounted Btrfs filesystem.
pub struct Filesystem {
    device: Arc<dyn BlockRead>,
    sb: Superblock,
    map: ChunkMap,
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
        let mut sb_buf = vec![0u8; 4096];
        device.read_at(SUPER_INFO_OFFSET, &mut sb_buf)?;
        let sb = Superblock::parse_at(&sb_buf, SUPER_INFO_OFFSET)?;

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
            sb,
            map,
            fs_tree_root,
            items: BTreeMap::new(),
        };
        fs.load_fs_tree()?;
        Ok(fs)
    }

    /// Read `buf.len()` bytes at a logical address through `map`.
    fn read_logical(
        device: &Arc<dyn BlockRead>,
        map: &ChunkMap,
        logical: u64,
        buf: &mut [u8],
    ) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let m = map.map(logical + done as u64)?;
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
    pub fn superblock(&self) -> &Superblock {
        &self.sb
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

        // Refuse rather than return the raw bytes: a compressed extent
        // read without decompressing looks exactly like a successful
        // read of corrupt data, which a caller cannot detect.
        if compression != 0 {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino}: extent uses compression type {compression}, which this driver \
                 does not decode"
            )));
        }
        if encryption != 0 || other != 0 {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino}: extent is encoded (encryption {encryption}, other {other})"
            )));
        }

        match kind {
            EXTENT_INLINE => {
                let end = data.len();
                let start = file_extent::INLINE_DATA.min(end);
                Ok(Piece::Inline(&data[start..end]))
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
                        Self::read_logical(&self.device, &self.map, logical, &mut out[at..at + n])?;
                    }
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
