//! B-tree nodes, leaves and traversal.
//!
//! # One structure, every tree
//!
//! Btrfs stores the chunk tree, the root tree, each subvolume's file
//! tree, the extent tree and the checksum tree in exactly the same
//! shape: a copy-on-write B-tree of fixed-size blocks, `nodesize` bytes
//! each, every block opening with the same [`Header`]. A block with
//! `level == 0` is a leaf and holds [`Item`]s; a block above level 0 is
//! an internal node and holds [`KeyPtr`]s naming child blocks. Nothing
//! in this module is specific to any one tree, which is why implementing
//! it once is enough to reach all of them.
//!
//! # Byte order
//!
//! Little-endian throughout, checksum included, like the rest of the
//! format.
//!
//! # Layout
//!
//! ```text
//!  0x00  +--------------------------------+
//!        | header (HEADER_SIZE bytes)     |
//!  0x65  +--------------------------------+  <- LEAF_DATA_OFFSET
//!        | item array / key_ptr array     |  grows forwards
//!        |            ...                 |
//!        |          free space            |
//!        |            ...                 |
//!        | item data, last item first     |  grows backwards
//! nodesize +------------------------------+
//! ```
//!
//! The two ends grow towards each other, so "the item array does not
//! reach the item data" is the leaf's integrity invariant and is checked
//! on every parse.
//!
//! **The `offset` field of an [`Item`] is measured from the end of the
//! header, not from the start of the block.** That single fact is the
//! easiest thing in this structure to get wrong, and getting it wrong
//! shifts every item's data by [`HEADER_SIZE`] bytes — far enough to
//! produce garbage, close enough to look like a plausible parse.
//!
//! # Trust
//!
//! A block is only parsed after it has proved three things about itself:
//! its checksum matches, its `bytenr` is the address the caller asked
//! for, and its `fsid` is this volume's. The checksum catches corrupted
//! bits. The other two catch an *intact* block that came from the wrong
//! place — a stale tree block left behind by an earlier filesystem, or a
//! logical-to-physical translation that quietly went wrong — which no
//! checksum can detect, because such a block's checksum is perfectly
//! good.
//!
//! # Confidence
//!
//! Offsets here were derived from the field order of
//! `struct btrfs_header`, `struct btrfs_key_ptr` and `struct btrfs_item`
//! in the published on-disk format, and every one of them is corroborated
//! by arithmetic that has to close: the three struct sizes named below
//! are the sums of their own field widths, and `HEADER_SIZE` is
//! independently confirmed by the fact that `nodesize - HEADER_SIZE` is
//! the leaf data area every item offset is relative to. Anything that
//! could not be corroborated twice is called out at its use site.

use std::cmp::Ordering;
use std::fmt;

use crate::chunk::{DiskKey, DISK_KEY_SIZE};
use crate::error::{Error, Result};
use crate::superblock::{
    le32, le64, uuid_at, ChecksumType, Superblock, CSUM_SIZE, MAX_LEVEL, UUID_SIZE,
};

/// Size of an on-disk `struct btrfs_header`.
///
/// The field widths sum to 101: csum 32, fsid 16, bytenr 8, flags 8,
/// chunk_tree_uuid 16, generation 8, owner 8, nritems 4, level 1. The
/// struct is packed, so there is no tail padding despite the odd
/// trailing byte.
pub const HEADER_SIZE: usize = 101;

/// Where a leaf's data area begins, and the origin every [`Item::offset`]
/// is measured from.
///
/// Identical to [`HEADER_SIZE`] — the item array and the item data share
/// one coordinate system that starts immediately after the header — but
/// named separately because the two are conceptually different things
/// and conflating them is how the offset origin gets lost.
pub const LEAF_DATA_OFFSET: usize = HEADER_SIZE;

/// Size of an on-disk `struct btrfs_key_ptr`: a 17-byte key, then
/// `blockptr` and `generation`.
pub const KEY_PTR_SIZE: usize = DISK_KEY_SIZE + 8 + 8;

/// Size of an on-disk `struct btrfs_item`: a 17-byte key, then a 32-bit
/// `offset` and a 32-bit `size`.
pub const ITEM_SIZE: usize = DISK_KEY_SIZE + 4 + 4;

/// Byte offsets within a `struct btrfs_header`.
///
/// The first four fields deliberately mirror the superblock's opening
/// layout — csum, fsid, bytenr, flags at the same offsets — which is a
/// useful sanity anchor: [`crate::superblock::offsets::CSUM`],
/// `FSID` and `BYTENR` carry the same values as the constants below.
pub mod header_offsets {
    /// Checksum of bytes `0x20 .. nodesize`.
    pub const CSUM: usize = 0x00;
    /// Filesystem UUID; `metadata_uuid` when that feature is enabled.
    pub const FSID: usize = 0x20;
    /// Logical address this block belongs at.
    pub const BYTENR: usize = 0x30;
    /// Header flags — see [`super::header_flags`].
    pub const FLAGS: usize = 0x38;
    /// UUID of the chunk tree this block's addresses were allocated
    /// under.
    pub const CHUNK_TREE_UUID: usize = 0x40;
    /// Transaction id that wrote this block.
    pub const GENERATION: usize = 0x50;
    /// Objectid of the tree that owns this block.
    pub const OWNER: usize = 0x58;
    /// Number of key pointers (node) or items (leaf) that follow.
    pub const NRITEMS: usize = 0x60;
    /// Height above the leaves; 0 means this block *is* a leaf.
    pub const LEVEL: usize = 0x64;
}

/// Header `flags` bits.
pub mod header_flags {
    /// The block has been written out at least once.
    pub const WRITTEN: u64 = 1 << 0;
    /// The block belongs to a relocation tree.
    pub const RELOC: u64 = 1 << 1;
    /// Bit position of the back-reference revision, which shares the
    /// `flags` word with the bits above and occupies its top byte.
    pub const BACKREF_REV_SHIFT: u32 = 56;
    /// Mask selecting the back-reference revision.
    pub const BACKREF_REV_MASK: u64 = 0xffu64 << BACKREF_REV_SHIFT;
    /// Back-reference revision predating the `MIXED_BACKREF` feature.
    pub const OLD_BACKREF_REV: u8 = 0;
    /// Back-reference revision used by every filesystem with
    /// `MIXED_BACKREF` set, which is every modern one.
    pub const MIXED_BACKREF_REV: u8 = 1;
}

/// Compare two keys the way Btrfs orders them: `objectid`, then
/// `key_type`, then `offset`, each unsigned.
///
/// This ordering is the whole contract of the B-tree — a search that
/// compares in a different order will descend into the wrong subtree and
/// report a perfectly real item as missing.
pub fn compare_keys(a: &DiskKey, b: &DiskKey) -> Ordering {
    a.objectid
        .cmp(&b.objectid)
        .then(a.key_type.cmp(&b.key_type))
        .then(a.offset.cmp(&b.offset))
}

/// Report a tree block as structurally malformed.
///
/// [`Error`] has no `BadTreeBlock` variant, so these go out as
/// [`Error::BadSuperblock`], whose *documented* meaning — "the volume is
/// Btrfs, but a structural field is out of range or internally
/// inconsistent" — fits even though its name does not. Every message is
/// prefixed with the offending block's logical address so a log line is
/// not mistaken for a complaint about the superblock itself. A dedicated
/// variant would be better and is worth adding the next time `error.rs`
/// is open.
fn bad_block(logical: u64, msg: impl fmt::Display) -> Error {
    Error::BadSuperblock(format!("tree block at {logical:#x}: {msg}"))
}

/// Render a UUID as plain hex, for error messages.
fn hex_uuid(u: &[u8; UUID_SIZE]) -> String {
    let mut s = String::with_capacity(UUID_SIZE * 2);
    for b in u {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// The `struct btrfs_header` that opens every tree block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// The raw 32-byte checksum field, as stored. Only the first
    /// [`ChecksumType::digest_len`] bytes are meaningful.
    pub csum: [u8; CSUM_SIZE],
    /// The filesystem this block belongs to. Holds `metadata_uuid` when
    /// the `METADATA_UUID` feature is set, which is exactly what
    /// [`Superblock::node_uuid`] returns.
    pub fsid: [u8; UUID_SIZE],
    /// The logical address this block belongs at.
    pub bytenr: u64,
    /// Header flags — see [`header_flags`].
    pub flags: u64,
    /// UUID of the chunk tree in force when this block was allocated.
    pub chunk_tree_uuid: [u8; UUID_SIZE],
    /// Transaction id that wrote this block.
    pub generation: u64,
    /// Objectid of the owning tree — 1 for the root tree, 3 for the
    /// chunk tree, 5 for the top-level file tree, and so on. See
    /// [`crate::chunk::objectid`].
    pub owner: u64,
    /// Number of key pointers (node) or items (leaf) in this block.
    pub nritems: u32,
    /// Height above the leaves. 0 means leaf.
    pub level: u8,
}

impl Header {
    /// Decode a header from the first [`HEADER_SIZE`] bytes of `buf`.
    ///
    /// This only decodes. Nothing is trusted until
    /// [`TreeBlock::parse`] has checked the checksum and the identity
    /// fields.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        use header_offsets as o;
        if buf.len() < HEADER_SIZE {
            return Err(Error::BadSuperblock(format!(
                "tree block header needs {HEADER_SIZE} bytes, got {}",
                buf.len()
            )));
        }
        Ok(Header {
            csum: buf[o::CSUM..o::CSUM + CSUM_SIZE]
                .try_into()
                .expect("32 bytes"),
            fsid: uuid_at(buf, o::FSID),
            bytenr: le64(buf, o::BYTENR),
            flags: le64(buf, o::FLAGS),
            chunk_tree_uuid: uuid_at(buf, o::CHUNK_TREE_UUID),
            generation: le64(buf, o::GENERATION),
            owner: le64(buf, o::OWNER),
            nritems: le32(buf, o::NRITEMS),
            level: buf[o::LEVEL],
        })
    }

    /// Whether this block is a leaf and therefore holds items rather
    /// than child pointers.
    pub fn is_leaf(&self) -> bool {
        self.level == 0
    }

    /// The back-reference revision packed into the top byte of `flags`.
    pub fn backref_rev(&self) -> u8 {
        ((self.flags & header_flags::BACKREF_REV_MASK) >> header_flags::BACKREF_REV_SHIFT) as u8
    }

    /// Whether the block has been written out at least once.
    pub fn is_written(&self) -> bool {
        self.flags & header_flags::WRITTEN != 0
    }

    /// Whether the block belongs to a relocation tree.
    pub fn is_reloc(&self) -> bool {
        self.flags & header_flags::RELOC != 0
    }
}

/// One `struct btrfs_key_ptr` — an internal node's pointer to a child
/// block.
///
/// `key` is the *smallest* key anywhere in the subtree the child roots,
/// which is what makes the descent rule work: to find a key, take the
/// last child whose key is less than or equal to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyPtr {
    /// Smallest key in the child subtree.
    pub key: DiskKey,
    /// Logical address of the child block.
    pub blockptr: u64,
    /// Transaction id the child was written in. Copy-on-write means a
    /// child older than its parent is normal; a child *newer* than the
    /// tree root is not.
    pub generation: u64,
}

impl KeyPtr {
    /// Parse a key pointer from the first [`KEY_PTR_SIZE`] bytes of
    /// `buf`.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < KEY_PTR_SIZE {
            return Err(Error::BadSuperblock(format!(
                "key pointer needs {KEY_PTR_SIZE} bytes, got {}",
                buf.len()
            )));
        }
        Ok(KeyPtr {
            key: DiskKey::parse(buf)?,
            // The struct is packed, so both u64s start on odd byte
            // offsets: 17 and 25.
            blockptr: le64(buf, DISK_KEY_SIZE),
            generation: le64(buf, DISK_KEY_SIZE + 8),
        })
    }
}

/// One `struct btrfs_item` — a leaf's index entry for a piece of item
/// data stored elsewhere in the same block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item {
    /// The key this item is filed under.
    pub key: DiskKey,
    /// Where the item's data starts, **measured from the end of the
    /// header** ([`LEAF_DATA_OFFSET`]), not from the start of the block.
    pub offset: u32,
    /// How many bytes of data the item has.
    pub size: u32,
}

impl Item {
    /// Parse an item from the first [`ITEM_SIZE`] bytes of `buf`.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < ITEM_SIZE {
            return Err(Error::BadSuperblock(format!(
                "leaf item needs {ITEM_SIZE} bytes, got {}",
                buf.len()
            )));
        }
        Ok(Item {
            key: DiskKey::parse(buf)?,
            // Packed struct again: these two u32s start at 17 and 21.
            offset: le32(buf, DISK_KEY_SIZE),
            size: le32(buf, DISK_KEY_SIZE + 4),
        })
    }

    /// One past the last byte of this item's data, in the same
    /// coordinate system as [`Item::offset`].
    pub fn data_end(&self) -> usize {
        self.offset as usize + self.size as usize
    }
}

/// A tree block's contents, which depend entirely on its level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// `level > 0` — pointers to child blocks, in ascending key order.
    Node(Vec<KeyPtr>),
    /// `level == 0` — the items themselves, in ascending key order.
    Leaf(Vec<Item>),
}

impl Body {
    /// The child pointers, or `None` if this is a leaf.
    pub fn key_ptrs(&self) -> Option<&[KeyPtr]> {
        match self {
            Body::Node(p) => Some(p),
            Body::Leaf(_) => None,
        }
    }

    /// The items, or `None` if this is an internal node.
    pub fn items(&self) -> Option<&[Item]> {
        match self {
            Body::Leaf(i) => Some(i),
            Body::Node(_) => None,
        }
    }

    /// How many entries the body holds, whichever kind they are.
    pub fn len(&self) -> usize {
        match self {
            Body::Node(p) => p.len(),
            Body::Leaf(i) => i.len(),
        }
    }

    /// Whether the body holds no entries at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Everything needed to validate a tree block, gathered from the
/// superblock once so the walker does not have to carry it around.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeGeometry {
    /// Size of every metadata block, in bytes.
    pub nodesize: u32,
    /// Algorithm protecting each block.
    pub csum_type: ChecksumType,
    /// UUID every block header must carry.
    pub fsid: [u8; UUID_SIZE],
}

impl TreeGeometry {
    /// Take the geometry from a parsed superblock.
    ///
    /// The UUID comes from [`Superblock::node_uuid`] rather than
    /// `fsid`: when the `METADATA_UUID` feature is set the volume's
    /// visible UUID and the one stamped into tree blocks are
    /// deliberately different, and it is the latter a header carries.
    pub fn from_superblock(sb: &Superblock) -> Self {
        TreeGeometry {
            nodesize: sb.nodesize,
            csum_type: sb.csum_type,
            fsid: sb.node_uuid(),
        }
    }
}

/// A whole tree block: its header, its parsed body, and the bytes the
/// item data lives in.
///
/// Constructing one is the only way to get a [`Body`], and construction
/// verifies the checksum and both identity fields first, so a
/// `TreeBlock` in hand is a block that has already proved it is intact
/// and that it belongs where it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeBlock {
    /// The parsed header.
    pub header: Header,
    /// Child pointers or items, depending on the level.
    pub body: Body,
    /// The raw block, kept because leaf item data is referenced out of
    /// it by offset.
    bytes: Vec<u8>,
}

impl TreeBlock {
    /// Verify and parse a `nodesize`-byte block read from `logical`.
    ///
    /// # Errors
    ///
    /// [`Error::ChecksumMismatch`] if the stored digest disagrees,
    /// [`Error::BlockIdentityMismatch`] if the header's `bytenr` is not
    /// `logical`, and [`Error::BadSuperblock`] — see [`bad_block`] — if
    /// the fsid is foreign or the block's internal geometry does not
    /// hold up.
    pub fn parse(bytes: Vec<u8>, logical: u64, geom: &TreeGeometry) -> Result<Self> {
        let nodesize = geom.nodesize as usize;
        if bytes.len() != nodesize {
            return Err(bad_block(
                logical,
                format!("got {} bytes for a {nodesize}-byte node", bytes.len()),
            ));
        }
        let header = Header::parse(&bytes)?;

        // Checksum first: nothing below is worth reading if the bits are
        // not the bits that were written. The digest covers
        // `CSUM_SIZE .. nodesize` — the whole block minus the checksum
        // field, exactly the same span rule the superblock uses, just
        // over nodesize instead of 4 KiB.
        if !geom
            .csum_type
            .verify(&bytes[CSUM_SIZE..nodesize], &header.csum)
        {
            return Err(Error::ChecksumMismatch {
                what: "tree block",
                offset: logical,
            });
        }

        // Then identity. An intact block from the wrong place has a
        // perfectly good checksum, so these two checks are the only
        // thing standing between a mistranslated logical address and a
        // confidently wrong answer.
        if header.bytenr != logical {
            return Err(Error::BlockIdentityMismatch {
                what: "tree block",
                expected: logical,
                found: header.bytenr,
            });
        }
        if header.fsid != geom.fsid {
            // Not BlockIdentityMismatch: that variant carries u64
            // addresses, and squeezing a UUID into one would produce an
            // error message that reads like nonsense.
            return Err(bad_block(
                logical,
                format!(
                    "fsid {} is not this volume's {}",
                    hex_uuid(&header.fsid),
                    hex_uuid(&geom.fsid)
                ),
            ));
        }
        if header.level >= MAX_LEVEL {
            return Err(bad_block(
                logical,
                format!(
                    "level {} is at or above the maximum tree height {MAX_LEVEL}",
                    header.level
                ),
            ));
        }

        let nritems = header.nritems as usize;
        let body = if header.is_leaf() {
            Body::Leaf(parse_leaf_items(&bytes, logical, nritems, nodesize)?)
        } else {
            Body::Node(parse_key_ptrs(&bytes, logical, nritems, nodesize)?)
        };

        Ok(TreeBlock {
            header,
            body,
            bytes,
        })
    }

    /// The raw block bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Whether this block is a leaf.
    pub fn is_leaf(&self) -> bool {
        self.header.is_leaf()
    }

    /// The data belonging to `item`.
    ///
    /// Returns `None` only if `item` did not come from this block:
    /// [`TreeBlock::parse`] bounds-checks every item it returns, so for
    /// those the lookup always succeeds.
    pub fn item_data(&self, item: &Item) -> Option<&[u8]> {
        let start = LEAF_DATA_OFFSET.checked_add(item.offset as usize)?;
        let end = start.checked_add(item.size as usize)?;
        self.bytes.get(start..end)
    }
}

/// Parse and validate a leaf's item array.
///
/// Three separate invariants are checked, and each one is a cheap
/// detector for a different mistake:
///
/// - every item's data lies inside the leaf data area, and starts after
///   the item array — catching a wrong offset origin or a corrupt size;
/// - the items' data runs are exactly contiguous, ending at the block's
///   end and butting up against each other — this is what the kernel
///   enforces on every leaf it reads, and it fails loudly if the item
///   stride or field offsets are wrong;
/// - keys strictly ascend — the B-tree's entire search contract.
fn parse_leaf_items(
    bytes: &[u8],
    logical: u64,
    nritems: usize,
    nodesize: usize,
) -> Result<Vec<Item>> {
    let array_end = nritems
        .checked_mul(ITEM_SIZE)
        .ok_or_else(|| bad_block(logical, format!("nritems {nritems} overflows")))?;
    if LEAF_DATA_OFFSET + array_end > nodesize {
        return Err(bad_block(
            logical,
            format!(
                "{nritems} items need {} bytes but the node is only {nodesize}",
                LEAF_DATA_OFFSET + array_end
            ),
        ));
    }
    let leaf_data_size = nodesize - LEAF_DATA_OFFSET;

    let mut items: Vec<Item> = Vec::with_capacity(nritems);
    // Item 0's data ends at the very end of the block; each subsequent
    // item's data ends where the previous one's began.
    let mut expected_end = leaf_data_size;
    for i in 0..nritems {
        let at = LEAF_DATA_OFFSET + i * ITEM_SIZE;
        let item = Item::parse(&bytes[at..])
            .map_err(|_| bad_block(logical, format!("item {i} is truncated at byte {at}")))?;
        let end = item.data_end();
        if end > leaf_data_size {
            return Err(bad_block(
                logical,
                format!(
                    "item {i} data [{}, {end}) runs past the {leaf_data_size}-byte data area",
                    item.offset
                ),
            ));
        }
        if (item.offset as usize) < array_end {
            return Err(bad_block(
                logical,
                format!(
                    "item {i} data starts at {}, inside the item array that ends at {array_end}",
                    item.offset
                ),
            ));
        }
        if end != expected_end {
            return Err(bad_block(
                logical,
                format!("item {i} data ends at {end}, expected {expected_end} — items are not contiguous"),
            ));
        }
        expected_end = item.offset as usize;
        if let Some(prev) = items.last() {
            if compare_keys(&prev.key, &item.key) != Ordering::Less {
                return Err(bad_block(
                    logical,
                    format!("item {i} key {:?} does not follow {:?}", item.key, prev.key),
                ));
            }
        }
        items.push(item);
    }
    Ok(items)
}

/// Parse and validate an internal node's key-pointer array.
fn parse_key_ptrs(
    bytes: &[u8],
    logical: u64,
    nritems: usize,
    nodesize: usize,
) -> Result<Vec<KeyPtr>> {
    if nritems == 0 {
        // A node with no children cannot be descended and cannot have
        // been produced by a working allocator.
        return Err(bad_block(logical, "internal node has no children"));
    }
    let array_end = nritems
        .checked_mul(KEY_PTR_SIZE)
        .ok_or_else(|| bad_block(logical, format!("nritems {nritems} overflows")))?;
    if HEADER_SIZE + array_end > nodesize {
        return Err(bad_block(
            logical,
            format!(
                "{nritems} key pointers need {} bytes but the node is only {nodesize}",
                HEADER_SIZE + array_end
            ),
        ));
    }

    let mut ptrs: Vec<KeyPtr> = Vec::with_capacity(nritems);
    for i in 0..nritems {
        let at = HEADER_SIZE + i * KEY_PTR_SIZE;
        let ptr = KeyPtr::parse(&bytes[at..]).map_err(|_| {
            bad_block(
                logical,
                format!("key pointer {i} is truncated at byte {at}"),
            )
        })?;
        if ptr.blockptr == 0 {
            return Err(bad_block(logical, format!("child {i} has a null blockptr")));
        }
        if let Some(prev) = ptrs.last() {
            if compare_keys(&prev.key, &ptr.key) != Ordering::Less {
                return Err(bad_block(
                    logical,
                    format!("child {i} key {:?} does not follow {:?}", ptr.key, prev.key),
                ));
            }
        }
        ptrs.push(ptr);
    }
    Ok(ptrs)
}

/// The slot an internal node descends into when looking for `key`: the
/// last child whose key is less than or equal to `key`.
///
/// When `key` sorts before every child, slot 0 is used anyway. That is
/// not a fudge — it is what the reference implementation does, and it is
/// correct, because the leftmost subtree is where such a key would be
/// inserted and therefore the only place it could already be.
fn node_slot(ptrs: &[KeyPtr], key: &DiskKey) -> usize {
    let above = ptrs.partition_point(|p| compare_keys(&p.key, key) != Ordering::Greater);
    above.saturating_sub(1)
}

/// The slot a leaf search settles on: the first item whose key is
/// greater than or equal to `key`. Equal to `items.len()` when `key`
/// sorts after everything in the leaf.
fn leaf_slot(items: &[Item], key: &DiskKey) -> usize {
    items.partition_point(|i| compare_keys(&i.key, key) == Ordering::Less)
}

/// How the walker obtains bytes.
///
/// The caller supplies the device: given a logical address and a buffer
/// exactly `nodesize` long, fill the buffer. Keeping this a plain
/// closure rather than a device trait is deliberate — it lets the whole
/// traversal be exercised against an in-memory byte slice, and it keeps
/// this module from depending on the block-device layer.
pub type ReadBlock<'a> = &'a dyn Fn(u64, &mut [u8]) -> Result<()>;

/// One item lifted out of a leaf, with its data copied.
///
/// The copy is deliberate: the leaf it came from is a temporary, and an
/// item that borrowed from it would pin a whole `nodesize` block for the
/// sake of a few bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafItem {
    /// The key the item was filed under.
    pub key: DiskKey,
    /// The item's data.
    pub data: Vec<u8>,
}

/// A B-tree, reachable through a caller-supplied block reader.
///
/// One instance can walk any number of trees — the root address is a
/// per-call argument, not state — because every Btrfs tree shares this
/// exact structure.
pub struct Tree<'a> {
    geom: TreeGeometry,
    read: ReadBlock<'a>,
}

impl<'a> Tree<'a> {
    /// Build a walker over an explicit geometry.
    pub fn new(geom: TreeGeometry, read: ReadBlock<'a>) -> Self {
        Tree { geom, read }
    }

    /// Build a walker using the geometry a parsed superblock implies.
    pub fn from_superblock(sb: &Superblock, read: ReadBlock<'a>) -> Self {
        Tree::new(TreeGeometry::from_superblock(sb), read)
    }

    /// The geometry this walker validates against.
    pub fn geometry(&self) -> &TreeGeometry {
        &self.geom
    }

    /// Read, verify and parse the block at `logical`.
    pub fn read_block(&self, logical: u64) -> Result<TreeBlock> {
        let mut buf = vec![0u8; self.geom.nodesize as usize];
        (self.read)(logical, &mut buf)?;
        TreeBlock::parse(buf, logical, &self.geom)
    }

    /// Descend from `root` to the leaf that would hold `key`.
    ///
    /// Returns the leaf whether or not the key is actually present —
    /// the leaf is where the caller looks next, and "not there" is a
    /// property of the leaf's contents, not of the descent.
    ///
    /// # Errors
    ///
    /// [`Error::BadSuperblock`] if the descent runs deeper than
    /// [`MAX_LEVEL`] blocks or a child's level does not sit exactly one
    /// below its parent's, plus anything [`TreeBlock::parse`] can
    /// report for each block along the way.
    pub fn descend(&self, root: u64, key: &DiskKey) -> Result<TreeBlock> {
        let mut logical = root;
        let mut expected_level: Option<u8> = None;
        // A Btrfs tree is at most MAX_LEVEL blocks tall, so this many
        // reads is enough for any well-formed tree and few enough that a
        // cycle in a corrupt one cannot spin forever.
        for _ in 0..MAX_LEVEL {
            let block = self.read_block(logical)?;
            if let Some(want) = expected_level {
                if block.header.level != want {
                    return Err(bad_block(
                        logical,
                        format!(
                            "level {} where the parent said {want} — the tree is inconsistent",
                            block.header.level
                        ),
                    ));
                }
            }
            let Some(ptrs) = block.body.key_ptrs() else {
                return Ok(block);
            };
            let slot = node_slot(ptrs, key);
            expected_level = Some(block.header.level - 1);
            logical = ptrs[slot].blockptr;
        }
        Err(bad_block(
            root,
            format!("descent did not reach a leaf within {MAX_LEVEL} levels"),
        ))
    }

    /// Find the item filed under exactly `key`, if the tree holds one.
    ///
    /// Keys are unique within a tree, so an exact match is at most one
    /// item. Use [`Tree::find_all`] for the far commoner case of wanting
    /// every item sharing an objectid and type.
    pub fn search(&self, root: u64, key: &DiskKey) -> Result<Option<LeafItem>> {
        let leaf = self.descend(root, key)?;
        let items = leaf
            .body
            .items()
            .ok_or_else(|| bad_block(root, "descent ended on an internal node"))?;
        let slot = leaf_slot(items, key);
        let Some(item) = items.get(slot) else {
            return Ok(None);
        };
        if compare_keys(&item.key, key) != Ordering::Equal {
            return Ok(None);
        }
        let data = leaf
            .item_data(item)
            .ok_or_else(|| bad_block(leaf.header.bytenr, "item data is out of bounds"))?;
        Ok(Some(LeafItem {
            key: item.key,
            data: data.to_vec(),
        }))
    }

    /// Every item in the tree sharing `objectid` and `key_type`, in key
    /// order.
    ///
    /// This is the shape almost every real lookup takes: a directory's
    /// entries, a file's extents and an inode's extended attributes are
    /// each a run of items under one objectid and one type, distinguished
    /// only by their key offset.
    pub fn find_all(&self, root: u64, objectid: u64, key_type: u8) -> Result<Vec<LeafItem>> {
        let start = DiskKey {
            objectid,
            key_type,
            offset: 0,
        };
        let mut out = Vec::new();
        self.for_each_from(root, &start, &mut |key, data| {
            if key.objectid != objectid || key.key_type != key_type {
                // Keys ascend, so the first key past the run ends it.
                return Ok(false);
            }
            out.push(LeafItem {
                key: *key,
                data: data.to_vec(),
            });
            Ok(true)
        })?;
        Ok(out)
    }

    /// Visit every item in the tree, in key order.
    ///
    /// `visit` returns `false` to stop early. Item data is passed by
    /// reference straight out of the leaf, so a caller that only needs
    /// to inspect a few bytes never copies the rest.
    pub fn for_each(
        &self,
        root: u64,
        visit: &mut dyn FnMut(&DiskKey, &[u8]) -> Result<bool>,
    ) -> Result<()> {
        self.walk(root, None, 0, visit).map(|_| ())
    }

    /// Visit every item from `start` onwards, in key order.
    ///
    /// The tree is descended to `start` rather than scanned from the
    /// beginning, so seeking deep into a large tree costs one block read
    /// per level and nothing more.
    pub fn for_each_from(
        &self,
        root: u64,
        start: &DiskKey,
        visit: &mut dyn FnMut(&DiskKey, &[u8]) -> Result<bool>,
    ) -> Result<()> {
        self.walk(root, Some(start), 0, visit).map(|_| ())
    }

    /// Depth-first, left-to-right walk. Returns `false` once `visit` has
    /// asked to stop, so every caller up the recursion unwinds.
    ///
    /// `start` applies only to the leftmost path: once the walk has
    /// moved right of the starting position, every subsequent subtree is
    /// visited whole.
    fn walk(
        &self,
        logical: u64,
        start: Option<&DiskKey>,
        depth: u8,
        visit: &mut dyn FnMut(&DiskKey, &[u8]) -> Result<bool>,
    ) -> Result<bool> {
        if depth >= MAX_LEVEL {
            return Err(bad_block(
                logical,
                format!("walk went deeper than {MAX_LEVEL} levels"),
            ));
        }
        let block = self.read_block(logical)?;
        match &block.body {
            Body::Leaf(items) => {
                let first = start.map_or(0, |k| leaf_slot(items, k));
                for item in items.iter().skip(first) {
                    let data = block.item_data(item).ok_or_else(|| {
                        bad_block(block.header.bytenr, "item data is out of bounds")
                    })?;
                    if !visit(&item.key, data)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Body::Node(ptrs) => {
                let first = start.map_or(0, |k| node_slot(ptrs, k));
                for (i, ptr) in ptrs.iter().enumerate().skip(first) {
                    // Only the subtree the start key lands in needs to be
                    // entered partway; everything to its right is whole.
                    let child_start = if i == first { start } else { None };
                    if !self.walk(ptr.blockptr, child_start, depth + 1, visit)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests over hand-built tree blocks.
    //!
    //! **Necessary but not sufficient.** Every block below is encoded by
    //! this test module using the same offsets, strides and byte order
    //! the parser decodes with, so a misreading of `struct btrfs_header`,
    //! `struct btrfs_item` or `struct btrfs_key_ptr` would be baked into
    //! both sides and these tests would pass anyway. What they do buy is
    //! coverage of the bounds arithmetic, the key ordering rules and the
    //! descent logic — things that are wrong or right independently of
    //! the field offsets. Whether the field offsets themselves are right
    //! is settled only by `tests/btree_oracle.rs`, which parses blocks
    //! that `mkfs.btrfs` wrote.

    use super::*;
    use crate::chunk::objectid;
    use std::collections::HashMap;

    const NODESIZE: u32 = 4096;
    const FSID: [u8; UUID_SIZE] = [0x5A; UUID_SIZE];
    const CSUM: ChecksumType = ChecksumType::Crc32c;

    const LEAF_A: u64 = 0x1000;
    const LEAF_B: u64 = 0x2000;
    const ROOT: u64 = 0x3000;

    fn geom() -> TreeGeometry {
        TreeGeometry {
            nodesize: NODESIZE,
            csum_type: CSUM,
            fsid: FSID,
        }
    }

    fn key(objectid: u64, key_type: u8, offset: u64) -> DiskKey {
        DiskKey {
            objectid,
            key_type,
            offset,
        }
    }

    fn put_key(b: &mut [u8], at: usize, k: &DiskKey) {
        b[at..at + 8].copy_from_slice(&k.objectid.to_le_bytes());
        b[at + 8] = k.key_type;
        b[at + 9..at + 17].copy_from_slice(&k.offset.to_le_bytes());
    }

    /// Write a header. Leaves the checksum for [`seal`].
    fn put_header(b: &mut [u8], bytenr: u64, owner: u64, nritems: u32, level: u8) {
        use header_offsets as o;
        b[o::FSID..o::FSID + UUID_SIZE].copy_from_slice(&FSID);
        b[o::BYTENR..o::BYTENR + 8].copy_from_slice(&bytenr.to_le_bytes());
        let flags = (u64::from(header_flags::MIXED_BACKREF_REV) << header_flags::BACKREF_REV_SHIFT)
            | header_flags::WRITTEN;
        b[o::FLAGS..o::FLAGS + 8].copy_from_slice(&flags.to_le_bytes());
        b[o::CHUNK_TREE_UUID..o::CHUNK_TREE_UUID + UUID_SIZE].copy_from_slice(&[0xC4; UUID_SIZE]);
        b[o::GENERATION..o::GENERATION + 8].copy_from_slice(&9u64.to_le_bytes());
        b[o::OWNER..o::OWNER + 8].copy_from_slice(&owner.to_le_bytes());
        b[o::NRITEMS..o::NRITEMS + 4].copy_from_slice(&nritems.to_le_bytes());
        b[o::LEVEL] = level;
    }

    /// Recompute the block checksum after building or mutating a block.
    fn seal(b: &mut [u8]) {
        let digest = CSUM.digest(&b[CSUM_SIZE..]);
        b[..CSUM_SIZE].copy_from_slice(&digest);
    }

    /// Build a leaf holding `entries`, packed the way Btrfs packs them:
    /// item array forwards from the header, data backwards from the end.
    fn leaf(bytenr: u64, owner: u64, entries: &[(DiskKey, Vec<u8>)]) -> Vec<u8> {
        let mut b = vec![0u8; NODESIZE as usize];
        put_header(&mut b, bytenr, owner, entries.len() as u32, 0);
        let mut end = NODESIZE as usize - LEAF_DATA_OFFSET;
        for (i, (k, data)) in entries.iter().enumerate() {
            let offset = end - data.len();
            let at = LEAF_DATA_OFFSET + i * ITEM_SIZE;
            put_key(&mut b, at, k);
            b[at + DISK_KEY_SIZE..at + DISK_KEY_SIZE + 4]
                .copy_from_slice(&(offset as u32).to_le_bytes());
            b[at + DISK_KEY_SIZE + 4..at + DISK_KEY_SIZE + 8]
                .copy_from_slice(&(data.len() as u32).to_le_bytes());
            let start = LEAF_DATA_OFFSET + offset;
            b[start..start + data.len()].copy_from_slice(data);
            end = offset;
        }
        seal(&mut b);
        b
    }

    /// Build an internal node pointing at `children`.
    fn node(bytenr: u64, owner: u64, level: u8, children: &[(DiskKey, u64)]) -> Vec<u8> {
        let mut b = vec![0u8; NODESIZE as usize];
        put_header(&mut b, bytenr, owner, children.len() as u32, level);
        for (i, (k, child)) in children.iter().enumerate() {
            let at = HEADER_SIZE + i * KEY_PTR_SIZE;
            put_key(&mut b, at, k);
            b[at + DISK_KEY_SIZE..at + DISK_KEY_SIZE + 8].copy_from_slice(&child.to_le_bytes());
            b[at + DISK_KEY_SIZE + 8..at + DISK_KEY_SIZE + 16].copy_from_slice(&8u64.to_le_bytes());
        }
        seal(&mut b);
        b
    }

    /// A two-level tree: one root node over two leaves holding four
    /// items between them.
    ///
    /// ```text
    ///            root (level 1)
    ///          /                \
    ///  (1,1,0) leaf A        (5,1,0) leaf B
    ///  1:a  2:bb              5:ccc  9:dddd
    /// ```
    fn two_level_tree() -> HashMap<u64, Vec<u8>> {
        let a = leaf(
            LEAF_A,
            objectid::FS_TREE,
            &[
                (key(1, 1, 0), b"a".to_vec()),
                (key(2, 1, 0), b"bb".to_vec()),
            ],
        );
        let b = leaf(
            LEAF_B,
            objectid::FS_TREE,
            &[
                (key(5, 1, 0), b"ccc".to_vec()),
                (key(9, 1, 0), b"dddd".to_vec()),
            ],
        );
        let r = node(
            ROOT,
            objectid::FS_TREE,
            1,
            &[(key(1, 1, 0), LEAF_A), (key(5, 1, 0), LEAF_B)],
        );
        HashMap::from([(LEAF_A, a), (LEAF_B, b), (ROOT, r)])
    }

    /// Turn a block map into a reader closure.
    fn reader(blocks: &HashMap<u64, Vec<u8>>) -> impl Fn(u64, &mut [u8]) -> Result<()> + '_ {
        move |logical, buf| match blocks.get(&logical) {
            Some(block) => {
                buf.copy_from_slice(block);
                Ok(())
            }
            None => Err(Error::Io(format!("no block at {logical:#x}"))),
        }
    }

    fn collect(tree: &Tree<'_>, root: u64) -> Vec<(DiskKey, Vec<u8>)> {
        let mut out = Vec::new();
        tree.for_each(root, &mut |k, d| {
            out.push((*k, d.to_vec()));
            Ok(true)
        })
        .unwrap();
        out
    }

    #[test]
    fn struct_sizes_match_the_on_disk_layout() {
        // btrfs_header: csum, fsid, bytenr, flags, chunk_tree_uuid,
        // generation, owner, nritems, level — packed, no padding.
        assert_eq!(HEADER_SIZE, 32 + 16 + 8 + 8 + 16 + 8 + 8 + 4 + 1);
        assert_eq!(HEADER_SIZE, header_offsets::LEVEL + 1);
        // btrfs_key_ptr: key, blockptr, generation.
        assert_eq!(KEY_PTR_SIZE, 17 + 8 + 8);
        // btrfs_item: key, offset, size.
        assert_eq!(ITEM_SIZE, 17 + 4 + 4);
        // The header's first three fields sit where the superblock's do.
        assert_eq!(header_offsets::CSUM, crate::superblock::offsets::CSUM);
        assert_eq!(header_offsets::FSID, crate::superblock::offsets::FSID);
        assert_eq!(header_offsets::BYTENR, crate::superblock::offsets::BYTENR);
    }

    #[test]
    fn parses_and_verifies_a_leaf_header() {
        let bytes = leaf(
            LEAF_A,
            objectid::CHUNK_TREE,
            &[(key(7, 3, 11), vec![0xAB; 6])],
        );
        let block = TreeBlock::parse(bytes, LEAF_A, &geom()).unwrap();
        assert_eq!(block.header.bytenr, LEAF_A);
        assert_eq!(block.header.owner, objectid::CHUNK_TREE);
        assert_eq!(block.header.nritems, 1);
        assert_eq!(block.header.level, 0);
        assert!(block.header.is_leaf());
        assert!(block.header.is_written());
        assert!(!block.header.is_reloc());
        assert_eq!(block.header.backref_rev(), header_flags::MIXED_BACKREF_REV);
        let items = block.body.items().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].key, key(7, 3, 11));
        assert_eq!(block.item_data(&items[0]).unwrap(), &[0xAB; 6]);
    }

    #[test]
    fn parses_an_internal_node() {
        let bytes = node(
            ROOT,
            objectid::ROOT_TREE,
            2,
            &[(key(1, 1, 0), LEAF_A), (key(5, 1, 0), LEAF_B)],
        );
        let block = TreeBlock::parse(bytes, ROOT, &geom()).unwrap();
        assert!(!block.is_leaf());
        assert_eq!(block.header.level, 2);
        let ptrs = block.body.key_ptrs().unwrap();
        assert_eq!(ptrs.len(), 2);
        assert_eq!(ptrs[1].blockptr, LEAF_B);
        assert_eq!(ptrs[1].generation, 8);
        assert!(block.body.items().is_none());
        assert_eq!(block.body.len(), 2);
        assert!(!block.body.is_empty());
    }

    #[test]
    fn rejects_a_corrupted_block() {
        let mut bytes = leaf(LEAF_A, objectid::FS_TREE, &[(key(1, 1, 0), vec![1, 2, 3])]);
        // Flip a byte inside the checksummed span without resealing.
        bytes[HEADER_SIZE + 3] ^= 0xFF;
        assert!(matches!(
            TreeBlock::parse(bytes, LEAF_A, &geom()),
            Err(Error::ChecksumMismatch {
                what: "tree block",
                offset: LEAF_A
            })
        ));
    }

    #[test]
    fn rejects_an_intact_block_read_from_the_wrong_address() {
        // The block is perfectly valid — it simply is not the block the
        // caller asked for. No checksum can catch this.
        let bytes = leaf(LEAF_A, objectid::FS_TREE, &[(key(1, 1, 0), vec![1])]);
        assert!(matches!(
            TreeBlock::parse(bytes, LEAF_B, &geom()),
            Err(Error::BlockIdentityMismatch {
                what: "tree block",
                expected: LEAF_B,
                found: LEAF_A
            })
        ));
    }

    #[test]
    fn rejects_a_block_belonging_to_another_filesystem() {
        let mut bytes = leaf(LEAF_A, objectid::FS_TREE, &[(key(1, 1, 0), vec![1])]);
        bytes[header_offsets::FSID..header_offsets::FSID + UUID_SIZE].copy_from_slice(&[0x11; 16]);
        seal(&mut bytes);
        let err = TreeBlock::parse(bytes, LEAF_A, &geom()).unwrap_err();
        assert!(
            format!("{err}").contains("is not this volume's"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_a_level_above_the_maximum_tree_height() {
        let mut bytes = node(ROOT, objectid::FS_TREE, 1, &[(key(1, 1, 0), LEAF_A)]);
        bytes[header_offsets::LEVEL] = MAX_LEVEL;
        seal(&mut bytes);
        let err = TreeBlock::parse(bytes, ROOT, &geom()).unwrap_err();
        assert!(
            format!("{err}").contains("maximum tree height"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_an_item_count_that_cannot_fit_in_the_block() {
        let mut bytes = leaf(LEAF_A, objectid::FS_TREE, &[(key(1, 1, 0), vec![1])]);
        let too_many = (NODESIZE as usize / ITEM_SIZE) as u32 + 1;
        bytes[header_offsets::NRITEMS..header_offsets::NRITEMS + 4]
            .copy_from_slice(&too_many.to_le_bytes());
        seal(&mut bytes);
        let err = TreeBlock::parse(bytes, LEAF_A, &geom()).unwrap_err();
        assert!(
            format!("{err}").contains("but the node is only"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_item_data_running_past_the_end_of_the_leaf() {
        let mut bytes = leaf(LEAF_A, objectid::FS_TREE, &[(key(1, 1, 0), vec![1, 2, 3])]);
        // Grow the item's size so offset + size leaves the data area.
        let size_at = LEAF_DATA_OFFSET + DISK_KEY_SIZE + 4;
        bytes[size_at..size_at + 4].copy_from_slice(&9999u32.to_le_bytes());
        seal(&mut bytes);
        let err = TreeBlock::parse(bytes, LEAF_A, &geom()).unwrap_err();
        assert!(
            format!("{err}").contains("runs past"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_item_data_overlapping_the_item_array() {
        // Two items whose data is pushed down to offset 0, where the
        // item array itself lives.
        let mut bytes = leaf(
            LEAF_A,
            objectid::FS_TREE,
            &[(key(1, 1, 0), vec![0; 8]), (key(2, 1, 0), vec![0; 8])],
        );
        let at = LEAF_DATA_OFFSET + ITEM_SIZE;
        bytes[at + DISK_KEY_SIZE..at + DISK_KEY_SIZE + 4].copy_from_slice(&0u32.to_le_bytes());
        bytes[at + DISK_KEY_SIZE + 4..at + DISK_KEY_SIZE + 8].copy_from_slice(&8u32.to_le_bytes());
        seal(&mut bytes);
        let err = TreeBlock::parse(bytes, LEAF_A, &geom()).unwrap_err();
        assert!(
            format!("{err}").contains("inside the item array"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_a_leaf_whose_keys_do_not_ascend() {
        let bytes = leaf(
            LEAF_A,
            objectid::FS_TREE,
            &[(key(9, 1, 0), vec![1]), (key(2, 1, 0), vec![2])],
        );
        let err = TreeBlock::parse(bytes, LEAF_A, &geom()).unwrap_err();
        assert!(
            format!("{err}").contains("does not follow"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_an_internal_node_with_no_children() {
        let bytes = node(ROOT, objectid::FS_TREE, 1, &[]);
        let err = TreeBlock::parse(bytes, ROOT, &geom()).unwrap_err();
        assert!(
            format!("{err}").contains("no children"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn compares_keys_objectid_then_type_then_offset() {
        assert_eq!(compare_keys(&key(1, 9, 9), &key(2, 0, 0)), Ordering::Less);
        assert_eq!(compare_keys(&key(1, 1, 9), &key(1, 2, 0)), Ordering::Less);
        assert_eq!(compare_keys(&key(1, 1, 1), &key(1, 1, 2)), Ordering::Less);
        assert_eq!(compare_keys(&key(1, 1, 1), &key(1, 1, 1)), Ordering::Equal);
        // u64, not i64: an objectid with the top bit set sorts last.
        assert_eq!(
            compare_keys(&key(u64::MAX, 0, 0), &key(1, 0, 0)),
            Ordering::Greater
        );
    }

    #[test]
    fn walks_a_two_level_tree_in_key_order() {
        let blocks = two_level_tree();
        let read = reader(&blocks);
        let tree = Tree::new(geom(), &read);
        let items = collect(&tree, ROOT);
        assert_eq!(
            items.iter().map(|(k, _)| k.objectid).collect::<Vec<_>>(),
            vec![1, 2, 5, 9]
        );
        assert_eq!(items[3].1, b"dddd");
    }

    #[test]
    fn finds_an_item_in_each_leaf() {
        let blocks = two_level_tree();
        let read = reader(&blocks);
        let tree = Tree::new(geom(), &read);
        assert_eq!(
            tree.search(ROOT, &key(2, 1, 0)).unwrap().unwrap().data,
            b"bb"
        );
        assert_eq!(
            tree.search(ROOT, &key(9, 1, 0)).unwrap().unwrap().data,
            b"dddd"
        );
    }

    #[test]
    fn reports_a_missing_key_rather_than_the_neighbouring_one() {
        let blocks = two_level_tree();
        let read = reader(&blocks);
        let tree = Tree::new(geom(), &read);
        // Between two items in the same leaf.
        assert!(tree.search(ROOT, &key(3, 1, 0)).unwrap().is_none());
        // Before everything in the tree — the descent still has to pick
        // slot 0 rather than underflowing.
        assert!(tree.search(ROOT, &key(0, 0, 0)).unwrap().is_none());
        // After everything in the tree.
        assert!(tree.search(ROOT, &key(99, 1, 0)).unwrap().is_none());
        // Right key, wrong type.
        assert!(tree.search(ROOT, &key(5, 2, 0)).unwrap().is_none());
    }

    #[test]
    fn descends_to_the_leaf_that_would_hold_a_key() {
        let blocks = two_level_tree();
        let read = reader(&blocks);
        let tree = Tree::new(geom(), &read);
        assert_eq!(
            tree.descend(ROOT, &key(2, 1, 0)).unwrap().header.bytenr,
            LEAF_A
        );
        assert_eq!(
            tree.descend(ROOT, &key(7, 1, 0)).unwrap().header.bytenr,
            LEAF_B
        );
        // A key below every child still lands in the leftmost leaf.
        assert_eq!(
            tree.descend(ROOT, &key(0, 0, 0)).unwrap().header.bytenr,
            LEAF_A
        );
    }

    #[test]
    fn iterating_from_a_key_skips_everything_before_it() {
        let blocks = two_level_tree();
        let read = reader(&blocks);
        let tree = Tree::new(geom(), &read);
        let mut seen = Vec::new();
        tree.for_each_from(ROOT, &key(5, 1, 0), &mut |k, _| {
            seen.push(k.objectid);
            Ok(true)
        })
        .unwrap();
        assert_eq!(seen, vec![5, 9]);

        // A start key that falls between two leaves resumes at the first
        // item at or after it.
        let mut seen = Vec::new();
        tree.for_each_from(ROOT, &key(3, 0, 0), &mut |k, _| {
            seen.push(k.objectid);
            Ok(true)
        })
        .unwrap();
        assert_eq!(seen, vec![5, 9]);
    }

    #[test]
    fn a_visitor_can_stop_the_walk_early() {
        let blocks = two_level_tree();
        let read = reader(&blocks);
        let tree = Tree::new(geom(), &read);
        let mut seen = Vec::new();
        tree.for_each(ROOT, &mut |k, _| {
            seen.push(k.objectid);
            Ok(k.objectid < 2)
        })
        .unwrap();
        assert_eq!(seen, vec![1, 2]);
    }

    #[test]
    fn find_all_collects_one_objectid_and_type_and_stops() {
        // Three items under objectid 5: two of type 1 and one of type 2.
        // Only the run of type 1 may come back.
        let a = leaf(
            LEAF_A,
            objectid::FS_TREE,
            &[
                (key(4, 1, 0), b"before".to_vec()),
                (key(5, 1, 0), b"first".to_vec()),
            ],
        );
        let b = leaf(
            LEAF_B,
            objectid::FS_TREE,
            &[
                (key(5, 1, 7), b"second".to_vec()),
                (key(5, 2, 0), b"other type".to_vec()),
                (key(6, 1, 0), b"after".to_vec()),
            ],
        );
        let r = node(
            ROOT,
            objectid::FS_TREE,
            1,
            &[(key(4, 1, 0), LEAF_A), (key(5, 1, 7), LEAF_B)],
        );
        let blocks = HashMap::from([(LEAF_A, a), (LEAF_B, b), (ROOT, r)]);
        let read = reader(&blocks);
        let tree = Tree::new(geom(), &read);

        let found = tree.find_all(ROOT, 5, 1).unwrap();
        assert_eq!(
            found.iter().map(|i| i.key.offset).collect::<Vec<_>>(),
            vec![0, 7]
        );
        assert_eq!(found[1].data, b"second");
        assert!(tree.find_all(ROOT, 5, 3).unwrap().is_empty());
    }

    #[test]
    fn a_child_at_the_wrong_level_stops_the_descent() {
        // Root claims level 1, so its child must be a leaf. Point it at
        // another level-1 node instead.
        let mut blocks = two_level_tree();
        let bogus = node(LEAF_A, objectid::FS_TREE, 1, &[(key(1, 1, 0), LEAF_B)]);
        blocks.insert(LEAF_A, bogus);
        let read = reader(&blocks);
        let tree = Tree::new(geom(), &read);
        let err = tree.search(ROOT, &key(1, 1, 0)).unwrap_err();
        assert!(
            format!("{err}").contains("the tree is inconsistent"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_cycle_in_a_corrupt_tree_terminates() {
        // A node that points at itself would otherwise loop forever.
        let mut b = vec![0u8; NODESIZE as usize];
        put_header(&mut b, ROOT, objectid::FS_TREE, 1, 7);
        put_key(&mut b, HEADER_SIZE, &key(0, 0, 0));
        b[HEADER_SIZE + DISK_KEY_SIZE..HEADER_SIZE + DISK_KEY_SIZE + 8]
            .copy_from_slice(&ROOT.to_le_bytes());
        seal(&mut b);
        let blocks = HashMap::from([(ROOT, b)]);
        let read = reader(&blocks);
        let tree = Tree::new(geom(), &read);
        // The level check fires first, which is the tighter guard; the
        // depth cap behind it is what makes termination unconditional.
        assert!(tree.search(ROOT, &key(1, 1, 0)).is_err());
        assert!(tree.for_each(ROOT, &mut |_, _| Ok(true)).is_err());
    }

    #[test]
    fn a_short_read_is_rejected_rather_than_parsed() {
        let bytes = vec![0u8; NODESIZE as usize - 1];
        let err = TreeBlock::parse(bytes, LEAF_A, &geom()).unwrap_err();
        assert!(
            format!("{err}").contains("for a 4096-byte node"),
            "unexpected error: {err}"
        );
    }
}
