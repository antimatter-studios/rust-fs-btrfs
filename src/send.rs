//! Send streams: the format `btrfs send` writes and `btrfs receive`
//! replays (#273).
//!
//! A send stream is a subvolume flattened into a list of filesystem
//! operations -- make this file, write these bytes at that offset, set
//! this owner -- that a receiver performs to rebuild it somewhere else.
//! This module reads one ([`parse_send_stream`]) and writes one from a
//! read-only subvolume ([`Filesystem::send_subvolume`]).
//!
//! # The format
//!
//! From the published description of the stream (version 1), everything
//! little-endian:
//!
//! ```text
//! stream   "btrfs-stream\0" (13 bytes), u32 version, then commands
//! command  u32 len, u16 cmd, u32 crc, then `len` bytes of attributes
//! attr     u16 type, u16 len, then `len` bytes
//! ```
//!
//! `len` counts the attributes only, not the ten-byte header. `crc` is a
//! CRC-32C over the header and the attributes, taken with the `crc` field
//! zeroed, a seed of zero and no final inversion -- not the conventional
//! CRC-32C, which starts from and finishes with all ones. Integers in
//! attributes are `u64`; a timestamp is a `u64` of seconds and a `u32` of
//! nanoseconds; a UUID is its sixteen bytes.
//!
//! Version 2 changes one rule: a `DATA` attribute carries no length and
//! runs to the end of its command, so a write is no longer limited to what
//! a `u16` can count. It adds commands: `FALLOCATE`, which keeps a
//! preallocated range preallocated, and `ENCODED_WRITE`, which carries a
//! compressed extent's bytes as they are on disk for the receiver to store
//! without recompressing. Both versions are read and written here
//! ([`SendOptions`]); version 2's inode-flag command is carried through the
//! parser as raw attributes, and not written.
//!
//! # What a written stream holds
//!
//! A full stream ([`Filesystem::send_subvolume`]): no parent snapshot,
//! so every inode is created. The kernel creates each inode under a
//! temporary name and renames it into place; a receiver only needs each
//! command to make sense when it is applied, so here every inode is
//! created at its final path, parents before children. Then, per inode, its data (holes and preallocated
//! ranges are left unwritten, as the kernel leaves them), its extended
//! attributes and its size; owners and modes once everything exists; and
//! the times last and deepest first, because creating an entry moves its
//! directory's modification time.
//!
//! An incremental stream ([`Filesystem::send_subvolume_incremental`])
//! carries only what changed since a parent snapshot the receiver already
//! holds; [`incremental`] says how.
//!
//! The oracle is the other implementation: `tests/send_stream_kernel.rs`
//! parses streams the kernel's `btrfs send` wrote and has the guest's
//! `btrfs receive` replay streams this module wrote.

use std::collections::{BTreeMap, VecDeque};

pub mod incremental;

use crate::compression::Compression;
use crate::error::{Error, Result};
use crate::fs::{file_extent, root_item, Filesystem, EXTENT_DATA_KEY, ROOT_ITEM_KEY};
use crate::inode::{FileType, Inode, Timestamp};
use crate::subvol::is_subvolume_id;
use crate::superblock::le64;

/// The thirteen bytes every send stream opens with.
pub const SEND_STREAM_MAGIC: &[u8; 13] = b"btrfs-stream\0";

/// The size of a command's header: `len`, `cmd`, `crc`.
pub const COMMAND_HEADER_LEN: usize = 10;

/// The size of an attribute's header: `type`, `len`.
pub const ATTR_HEADER_LEN: usize = 4;

/// The most file data one version-1 `WRITE` carries.
///
/// A receiver reads each command into a buffer of 64 KiB, header and
/// path included, so the data has to leave room for both; 48 KiB is what
/// the kernel puts in each.
pub const SEND_WRITE_CHUNK: usize = 48 * 1024;

/// A receiver's buffer for one version-1 command, header included.
pub const SEND_BUF_SIZE_V1: usize = 64 * 1024;

/// Command numbers.
pub mod cmd {
    /// Create the subvolume the stream rebuilds.
    pub const SUBVOL: u16 = 1;
    /// Create it as a snapshot of one the receiver already has.
    pub const SNAPSHOT: u16 = 2;
    /// Create a regular file.
    pub const MKFILE: u16 = 3;
    /// Create a directory.
    pub const MKDIR: u16 = 4;
    /// Create a device node.
    pub const MKNOD: u16 = 5;
    /// Create a FIFO.
    pub const MKFIFO: u16 = 6;
    /// Create a socket.
    pub const MKSOCK: u16 = 7;
    /// Create a symbolic link.
    pub const SYMLINK: u16 = 8;
    /// Rename.
    pub const RENAME: u16 = 9;
    /// Add a hard link.
    pub const LINK: u16 = 10;
    /// Remove a name.
    pub const UNLINK: u16 = 11;
    /// Remove a directory.
    pub const RMDIR: u16 = 12;
    /// Set an extended attribute.
    pub const SET_XATTR: u16 = 13;
    /// Remove an extended attribute.
    pub const REMOVE_XATTR: u16 = 14;
    /// Write file data.
    pub const WRITE: u16 = 15;
    /// Clone a range from a file the receiver has.
    pub const CLONE: u16 = 16;
    /// Set a file's size.
    pub const TRUNCATE: u16 = 17;
    /// Set the permission bits.
    pub const CHMOD: u16 = 18;
    /// Set the owner and group.
    pub const CHOWN: u16 = 19;
    /// Set the times.
    pub const UTIMES: u16 = 20;
    /// The end of the stream.
    pub const END: u16 = 21;
    /// A range changed, without its data (`btrfs send --no-data`).
    pub const UPDATE_EXTENT: u16 = 22;
    /// Preallocate or punch a range (version 2).
    pub const FALLOCATE: u16 = 23;
    /// Set the inode flags (version 2).
    pub const FILEATTR: u16 = 24;
    /// Write data still compressed (version 2).
    pub const ENCODED_WRITE: u16 = 25;
    /// Enable fs-verity (version 3).
    pub const ENABLE_VERITY: u16 = 26;
}

/// Attribute numbers.
pub mod attr {
    /// A UUID, sixteen bytes.
    pub const UUID: u16 = 1;
    /// A subvolume's change transaction.
    pub const CTRANSID: u16 = 2;
    /// An inode number.
    pub const INO: u16 = 3;
    /// A file size.
    pub const SIZE: u16 = 4;
    /// A mode word.
    pub const MODE: u16 = 5;
    /// An owner.
    pub const UID: u16 = 6;
    /// A group.
    pub const GID: u16 = 7;
    /// A device number.
    pub const RDEV: u16 = 8;
    /// Change time.
    pub const CTIME: u16 = 9;
    /// Modification time.
    pub const MTIME: u16 = 10;
    /// Access time.
    pub const ATIME: u16 = 11;
    /// Creation time.
    pub const OTIME: u16 = 12;
    /// An extended attribute's name.
    pub const XATTR_NAME: u16 = 13;
    /// An extended attribute's value.
    pub const XATTR_DATA: u16 = 14;
    /// The path a command acts on.
    pub const PATH: u16 = 15;
    /// A rename's destination.
    pub const PATH_TO: u16 = 16;
    /// A link's target, or a symlink's.
    pub const PATH_LINK: u16 = 17;
    /// An offset within a file.
    pub const FILE_OFFSET: u16 = 18;
    /// File data.
    pub const DATA: u16 = 19;
    /// A clone source's subvolume UUID.
    pub const CLONE_UUID: u16 = 20;
    /// A clone source's subvolume transaction.
    pub const CLONE_CTRANSID: u16 = 21;
    /// A clone source's path.
    pub const CLONE_PATH: u16 = 22;
    /// A clone source's offset.
    pub const CLONE_OFFSET: u16 = 23;
    /// How much a clone copies.
    pub const CLONE_LEN: u16 = 24;
    /// A `FALLOCATE`'s mode: `FALLOC_FL_*` bits, a `u32` (version 2).
    pub const FALLOCATE_MODE: u16 = 25;
    /// Inode flags (version 2).
    pub const FILEATTR: u16 = 26;
    /// How much of the file an encoded write fills (version 2).
    pub const UNENCODED_FILE_LEN: u16 = 27;
    /// What the encoded bytes decode to, all of it (version 2).
    pub const UNENCODED_LEN: u16 = 28;
    /// Where in the decoded bytes the file's range starts (version 2).
    pub const UNENCODED_OFFSET: u16 = 29;
    /// How an encoded write's bytes are compressed, a `u32` (version 2).
    pub const COMPRESSION: u16 = 30;
    /// How they are encrypted, a `u32`; always none (version 2).
    pub const ENCRYPTION: u16 = 31;
}

/// `FALLOC_FL_KEEP_SIZE`: preallocate without moving the file's size.
pub const FALLOC_FL_KEEP_SIZE: u32 = 1;

/// An encoded write's compression, as the stream numbers it: not the
/// number a file extent item stores. LZO is split by the sector size its
/// segments were cut to.
pub mod encoded {
    /// zlib.
    pub const ZLIB: u32 = 1;
    /// zstd.
    pub const ZSTD: u32 = 2;
    /// LZO in 4 KiB segments; 8, 16, 32 and 64 KiB follow in order.
    pub const LZO_4K: u32 = 3;
}

/// What kind of stream to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SendOptions {
    /// The stream version: 1, which every receiver reads, or 2.
    pub version: u32,
    /// Pass compressed extents through as `ENCODED_WRITE`s, as `btrfs
    /// send --compressed-data` does. Version 2 only.
    pub compressed_data: bool,
}

impl Default for SendOptions {
    fn default() -> Self {
        Self {
            version: 1,
            compressed_data: false,
        }
    }
}

impl SendOptions {
    /// A version-1 stream: what [`Filesystem::send_subvolume`] writes.
    pub fn v1() -> Self {
        Self::default()
    }

    /// A version-2 stream, compressed extents decoded and written plainly.
    pub fn v2() -> Self {
        Self {
            version: 2,
            ..Self::default()
        }
    }

    /// The same, passing compressed extents through or not.
    pub fn with_compressed_data(mut self, yes: bool) -> Self {
        self.compressed_data = yes;
        self
    }

    fn check(&self) -> Result<()> {
        if self.version != 1 && self.version != 2 {
            return Err(Error::UnsupportedFeature(format!(
                "send stream version {} is not one this writes",
                self.version
            )));
        }
        if self.compressed_data && self.version < 2 {
            return Err(Error::UnsupportedFeature(
                "compressed data passes through only in a version 2 stream".into(),
            ));
        }
        Ok(())
    }
}

/// What is wrong with a send stream.
///
/// Separate from the crate's [`Error`]: a stream is bytes handed in, not
/// a volume read, and "malformed" here says nothing about any filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamError {
    /// The stream is malformed: a wrong magic, a command or attribute that
    /// runs past its end, a missing or misshapen attribute, or no end
    /// command. Names the byte offset where it can.
    Malformed(String),
    /// A command's checksum disagrees with its bytes.
    ChecksumMismatch {
        /// Where the command starts in the stream.
        offset: u64,
    },
    /// A stream version this does not read.
    UnsupportedVersion(u32),
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::Malformed(m) => write!(f, "malformed send stream: {m}"),
            StreamError::ChecksumMismatch { offset } => {
                write!(
                    f,
                    "the send stream command at byte {offset} failed its checksum"
                )
            }
            StreamError::UnsupportedVersion(v) => {
                write!(f, "send stream version {v}; versions 1 and 2 are read")
            }
        }
    }
}

impl std::error::Error for StreamError {}

impl From<StreamError> for Error {
    /// Writing a stream fails this way only for a value no attribute can
    /// hold, which is a case the format cannot carry.
    fn from(e: StreamError) -> Self {
        Error::UnsupportedFeature(e.to_string())
    }
}

/// A result whose error is a [`StreamError`].
pub type StreamResult<T> = std::result::Result<T, StreamError>;

/// The name `btrfs receive --dump` prints for a command number.
pub fn command_name(cmd: u16) -> Option<&'static str> {
    Some(match cmd {
        cmd::SUBVOL => "subvol",
        cmd::SNAPSHOT => "snapshot",
        cmd::MKFILE => "mkfile",
        cmd::MKDIR => "mkdir",
        cmd::MKNOD => "mknod",
        cmd::MKFIFO => "mkfifo",
        cmd::MKSOCK => "mksock",
        cmd::SYMLINK => "symlink",
        cmd::RENAME => "rename",
        cmd::LINK => "link",
        cmd::UNLINK => "unlink",
        cmd::RMDIR => "rmdir",
        cmd::SET_XATTR => "set_xattr",
        cmd::REMOVE_XATTR => "remove_xattr",
        cmd::WRITE => "write",
        cmd::CLONE => "clone",
        cmd::TRUNCATE => "truncate",
        cmd::CHMOD => "chmod",
        cmd::CHOWN => "chown",
        cmd::UTIMES => "utimes",
        cmd::END => "end",
        cmd::UPDATE_EXTENT => "update_extent",
        cmd::FALLOCATE => "fallocate",
        cmd::FILEATTR => "fileattr",
        cmd::ENCODED_WRITE => "encoded_write",
        cmd::ENABLE_VERITY => "enable_verity",
        _ => return None,
    })
}

/// One command out of a stream, its attributes in the order they came.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// The command number; see [`cmd`].
    pub cmd: u16,
    /// `(type, value)` for each attribute; see [`attr`].
    pub attrs: Vec<(u16, Vec<u8>)>,
}

impl Command {
    /// The first attribute of type `ty`.
    pub fn attr(&self, ty: u16) -> Option<&[u8]> {
        self.attrs
            .iter()
            .find(|(t, _)| *t == ty)
            .map(|(_, v)| v.as_slice())
    }

    /// The attribute of type `ty`, which the command must carry.
    ///
    /// # Errors
    ///
    /// [`StreamError::Malformed`] when it is absent.
    pub fn require(&self, ty: u16) -> StreamResult<&[u8]> {
        self.attr(ty).ok_or_else(|| {
            StreamError::Malformed(format!(
                "a {} command has no attribute {ty}",
                command_name(self.cmd).unwrap_or("unknown")
            ))
        })
    }

    /// A `u64` attribute.
    ///
    /// # Errors
    ///
    /// [`StreamError::Malformed`] when it is absent or not eight bytes.
    pub fn u64(&self, ty: u16) -> StreamResult<u64> {
        let v = self.require(ty)?;
        let bytes: [u8; 8] = v.try_into().map_err(|_| {
            StreamError::Malformed(format!(
                "attribute {ty} of a {} command is {} bytes, not 8",
                command_name(self.cmd).unwrap_or("unknown"),
                v.len()
            ))
        })?;
        Ok(u64::from_le_bytes(bytes))
    }

    /// A timestamp attribute.
    ///
    /// # Errors
    ///
    /// [`StreamError::Malformed`] when it is absent or not twelve bytes.
    pub fn timestamp(&self, ty: u16) -> StreamResult<Timestamp> {
        let v = self.require(ty)?;
        if v.len() != 12 {
            return Err(StreamError::Malformed(format!(
                "timestamp attribute {ty} is {} bytes, not 12",
                v.len()
            )));
        }
        Ok(Timestamp {
            sec: i64::from_le_bytes(v[..8].try_into().expect("8 bytes")),
            nsec: u32::from_le_bytes(v[8..].try_into().expect("4 bytes")),
        })
    }

    /// The `PATH` attribute.
    ///
    /// # Errors
    ///
    /// [`StreamError::Malformed`] when it is absent.
    pub fn path(&self) -> StreamResult<&[u8]> {
        self.require(attr::PATH)
    }
}

/// A parsed stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendStream {
    /// The version in the stream header.
    pub version: u32,
    /// Every command, the closing `END` included.
    pub commands: Vec<Command>,
}

/// The stream's CRC-32C: seed zero, no final inversion.
///
/// The `crc32c` crate computes the conventional form, which inverts on
/// the way in and on the way out; undoing both gives the raw register.
pub fn stream_crc(bytes: &[u8]) -> u32 {
    !crc32c::crc32c_append(!0u32, bytes)
}

/// Parse a send stream and verify every command's checksum.
///
/// Stops at the `END` command; bytes after it are refused rather than
/// ignored, since a receiver that stopped there would silently drop them.
/// A stream that ends without one is refused as truncated. Version 1 and
/// version 2 framing are read; any other version is refused.
///
/// # Errors
///
/// [`StreamError::Malformed`] naming the offset of what is wrong,
/// [`StreamError::ChecksumMismatch`] for a command whose checksum
/// disagrees, and [`StreamError::UnsupportedVersion`] for a version this
/// does not read.
pub fn parse_send_stream(bytes: &[u8]) -> StreamResult<SendStream> {
    let header_len = SEND_STREAM_MAGIC.len() + 4;
    if bytes.len() < header_len || &bytes[..SEND_STREAM_MAGIC.len()] != SEND_STREAM_MAGIC {
        return Err(StreamError::Malformed(
            "the stream does not open with \"btrfs-stream\\0\"".into(),
        ));
    }
    let version = u32::from_le_bytes(
        bytes[SEND_STREAM_MAGIC.len()..header_len]
            .try_into()
            .expect("4 bytes"),
    );
    if version != 1 && version != 2 {
        return Err(StreamError::UnsupportedVersion(version));
    }

    let mut commands = Vec::new();
    let mut at = header_len;
    loop {
        if at == bytes.len() {
            return Err(StreamError::Malformed(format!(
                "the stream stops at byte {at} without an end command"
            )));
        }
        if bytes.len() - at < COMMAND_HEADER_LEN {
            return Err(StreamError::Malformed(format!(
                "a command header at byte {at} is cut short"
            )));
        }
        let len = u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes")) as usize;
        let cmd = u16::from_le_bytes(bytes[at + 4..at + 6].try_into().expect("2 bytes"));
        let crc = u32::from_le_bytes(bytes[at + 6..at + 10].try_into().expect("4 bytes"));
        let body_start = at + COMMAND_HEADER_LEN;
        let end = body_start
            .checked_add(len)
            .filter(|&e| e <= bytes.len())
            .ok_or_else(|| {
                StreamError::Malformed(format!(
                    "the command at byte {at} claims {len} bytes, past the end of the stream"
                ))
            })?;

        let mut framed = bytes[at..end].to_vec();
        framed[6..10].fill(0);
        if stream_crc(&framed) != crc {
            return Err(StreamError::ChecksumMismatch { offset: at as u64 });
        }

        let attrs = parse_attrs(&bytes[body_start..end], body_start, version)?;
        commands.push(Command { cmd, attrs });
        at = end;
        if cmd == cmd::END {
            break;
        }
    }
    if at != bytes.len() {
        return Err(StreamError::Malformed(format!(
            "{} bytes follow the end command at byte {at}",
            bytes.len() - at
        )));
    }
    Ok(SendStream { version, commands })
}

/// The attributes of one command. `base` is where `body` starts in the
/// stream, for the error messages.
fn parse_attrs(body: &[u8], base: usize, version: u32) -> StreamResult<Vec<(u16, Vec<u8>)>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < body.len() {
        if body.len() - at < 2 {
            return Err(StreamError::Malformed(format!(
                "an attribute header at byte {} is cut short",
                base + at
            )));
        }
        let ty = u16::from_le_bytes(body[at..at + 2].try_into().expect("2 bytes"));
        // Version 2's DATA has no length: it is the rest of the command.
        if version >= 2 && ty == attr::DATA {
            out.push((ty, body[at + 2..].to_vec()));
            break;
        }
        if body.len() - at < ATTR_HEADER_LEN {
            return Err(StreamError::Malformed(format!(
                "an attribute header at byte {} is cut short",
                base + at
            )));
        }
        let len = u16::from_le_bytes(body[at + 2..at + 4].try_into().expect("2 bytes")) as usize;
        let start = at + ATTR_HEADER_LEN;
        let end = start + len;
        if end > body.len() {
            return Err(StreamError::Malformed(format!(
                "attribute {ty} at byte {} claims {len} bytes, past the end of its command",
                base + at
            )));
        }
        out.push((ty, body[start..end].to_vec()));
        at = end;
    }
    Ok(out)
}

/// Builds a stream, one command at a time.
#[derive(Debug)]
pub struct StreamWriter {
    out: Vec<u8>,
    cmd: Option<(usize, u16)>,
    version: u32,
}

impl Default for StreamWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamWriter {
    /// A version-1 stream holding only its header.
    pub fn new() -> Self {
        Self::with_version(1)
    }

    /// A stream of `version` holding only its header. The writer does
    /// not check the number; [`SendOptions`] does.
    pub fn with_version(version: u32) -> Self {
        let mut out = SEND_STREAM_MAGIC.to_vec();
        out.extend_from_slice(&version.to_le_bytes());
        Self {
            out,
            cmd: None,
            version,
        }
    }

    /// Open a command. Attributes added until the next [`Self::begin`] or
    /// [`Self::finish`] belong to it.
    pub fn begin(&mut self, cmd: u16) {
        self.close();
        self.cmd = Some((self.out.len(), cmd));
        self.out.extend_from_slice(&[0; COMMAND_HEADER_LEN]);
    }

    /// Add an attribute to the open command.
    ///
    /// # Errors
    ///
    /// [`StreamError::Malformed`] for a value longer than a `u16` counts.
    pub fn attr(&mut self, ty: u16, value: &[u8]) -> StreamResult<()> {
        let len = u16::try_from(value.len()).map_err(|_| {
            StreamError::Malformed(format!(
                "attribute {ty} is {} bytes, more than an attribute holds",
                value.len()
            ))
        })?;
        self.out.extend_from_slice(&ty.to_le_bytes());
        self.out.extend_from_slice(&len.to_le_bytes());
        self.out.extend_from_slice(value);
        Ok(())
    }

    /// Add a `u64` attribute.
    pub fn attr_u64(&mut self, ty: u16, value: u64) -> StreamResult<()> {
        self.attr(ty, &value.to_le_bytes())
    }

    /// Add a `u32` attribute.
    pub fn attr_u32(&mut self, ty: u16, value: u32) -> StreamResult<()> {
        self.attr(ty, &value.to_le_bytes())
    }

    /// Add the `DATA` attribute: with a length in version 1; in version 2
    /// with none, running to the end of the command, so it has to be the
    /// command's last attribute.
    ///
    /// # Errors
    ///
    /// [`StreamError::Malformed`] in version 1 for more than a `u16`
    /// counts.
    pub fn attr_data(&mut self, value: &[u8]) -> StreamResult<()> {
        if self.version < 2 {
            return self.attr(attr::DATA, value);
        }
        self.out.extend_from_slice(&attr::DATA.to_le_bytes());
        self.out.extend_from_slice(value);
        Ok(())
    }

    /// Add a timestamp attribute.
    pub fn attr_time(&mut self, ty: u16, t: Timestamp) -> StreamResult<()> {
        let mut v = [0u8; 12];
        v[..8].copy_from_slice(&t.sec.to_le_bytes());
        v[8..].copy_from_slice(&t.nsec.to_le_bytes());
        self.attr(ty, &v)
    }

    /// Close the open command: fill in its length and checksum.
    fn close(&mut self) {
        let Some((at, cmd)) = self.cmd.take() else {
            return;
        };
        let len = (self.out.len() - at - COMMAND_HEADER_LEN) as u32;
        self.out[at..at + 4].copy_from_slice(&len.to_le_bytes());
        self.out[at + 4..at + 6].copy_from_slice(&cmd.to_le_bytes());
        let crc = stream_crc(&self.out[at..]);
        self.out[at + 6..at + 10].copy_from_slice(&crc.to_le_bytes());
    }

    /// Write the `END` command and hand back the stream.
    pub fn finish(mut self) -> Vec<u8> {
        self.begin(cmd::END);
        self.close();
        self.out
    }
}

/// `BTRFS_ROOT_SUBVOL_RDONLY`.
const ROOT_SUBVOL_RDONLY: u64 = 1;

/// Where in a `ROOT_ITEM` the subvolume's own UUID is.
const ROOT_ITEM_UUID: usize = root_item::GENERATION_V2 + 8;
/// `ctransid`: after the UUID, the parent's UUID and the received UUID.
const ROOT_ITEM_CTRANSID: usize = ROOT_ITEM_UUID + 3 * 16;

/// What a send of one subvolume needs from its `ROOT_ITEM`.
struct SubvolIdentity {
    name: Vec<u8>,
    uuid: [u8; 16],
    ctransid: u64,
    bytenr: u64,
}

/// One inode, met at its first path.
struct Pending {
    path: Vec<u8>,
    inode: Inode,
    depth: usize,
}

impl Filesystem {
    /// A full version-1 send stream of subvolume `id`, as `btrfs send`
    /// without `-p` writes one.
    ///
    /// The subvolume must be read-only, as the kernel requires: a stream
    /// of a tree that can change underneath it describes no single state.
    /// Subvolumes nested inside it are left out, as the kernel leaves
    /// them out. See the [module documentation](crate::send) for the order
    /// of the commands.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when no subvolume has that id,
    /// [`Error::UnsupportedFeature`] for one that is not read-only, and
    /// whatever reading the tree returns.
    pub fn send_subvolume(&self, id: u64) -> Result<Vec<u8>> {
        self.send_subvolume_with(id, SendOptions::default())
    }

    /// A full send stream of subvolume `id`, of the version and with the
    /// options `opts` names: [`Filesystem::send_subvolume`] is this with
    /// [`SendOptions::v1`].
    ///
    /// In version 2 a preallocated range is sent as a `FALLOCATE` that
    /// keeps the size, rather than left out, and with
    /// [`SendOptions::compressed_data`] a compressed extent is sent as an
    /// `ENCODED_WRITE` of its bytes as they are on disk, as `btrfs send
    /// --proto 2 --compressed-data` sends them.
    ///
    /// # Errors
    ///
    /// As [`Filesystem::send_subvolume`], and
    /// [`Error::UnsupportedFeature`] for options no stream can carry.
    pub fn send_subvolume_with(&self, id: u64, opts: SendOptions) -> Result<Vec<u8>> {
        opts.check()?;
        let subvol = self.send_identity(id)?;
        let tree = self.open_subvolume_at(subvol.bytenr)?;

        let mut w = StreamWriter::with_version(opts.version);
        w.begin(cmd::SUBVOL);
        w.attr(attr::PATH, &subvol.name)?;
        w.attr(attr::UUID, &subvol.uuid)?;
        w.attr_u64(attr::CTRANSID, subvol.ctransid)?;

        let root = tree.root_inode()?;
        let mut order = vec![Pending {
            path: Vec::new(),
            inode: root,
            depth: 0,
        }];
        let mut first_path: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        first_path.insert(order[0].inode.ino, Vec::new());

        let mut queue = VecDeque::from([(order[0].inode.ino, Vec::<u8>::new(), 0usize)]);
        while let Some((dir, dir_path, depth)) = queue.pop_front() {
            for entry in tree.read_dir(dir)? {
                if !entry.is_inode() {
                    continue; // a nested subvolume: not part of this stream
                }
                let mut path = dir_path.clone();
                if !path.is_empty() {
                    path.push(b'/');
                }
                path.extend_from_slice(&entry.name);

                if let Some(existing) = first_path.get(&entry.ino) {
                    w.begin(cmd::LINK);
                    w.attr(attr::PATH, &path)?;
                    w.attr(attr::PATH_LINK, existing)?;
                    continue;
                }
                let inode = tree.read_inode(entry.ino)?;
                self.send_create(&tree, &mut w, &path, &inode, opts)?;
                if inode.is_dir() {
                    queue.push_back((inode.ino, path.clone(), depth + 1));
                }
                first_path.insert(inode.ino, path.clone());
                order.push(Pending {
                    path,
                    inode,
                    depth: depth + 1,
                });
            }
        }

        // Owners and modes once every name exists; a mode after its owner,
        // because changing the owner clears set-user-ID.
        for p in &order {
            w.begin(cmd::CHOWN);
            w.attr(attr::PATH, &p.path)?;
            w.attr_u64(attr::UID, u64::from(p.inode.uid))?;
            w.attr_u64(attr::GID, u64::from(p.inode.gid))?;
            if !p.inode.is_symlink() {
                w.begin(cmd::CHMOD);
                w.attr(attr::PATH, &p.path)?;
                w.attr_u64(attr::MODE, u64::from(p.inode.permissions()))?;
            }
        }
        // Times last, deepest first: making an entry moves its directory's.
        let mut by_depth: Vec<&Pending> = order.iter().collect();
        by_depth.sort_by_key(|p| std::cmp::Reverse(p.depth));
        for p in by_depth {
            w.begin(cmd::UTIMES);
            w.attr(attr::PATH, &p.path)?;
            w.attr_time(attr::ATIME, p.inode.atime)?;
            w.attr_time(attr::MTIME, p.inode.mtime)?;
            w.attr_time(attr::CTIME, p.inode.ctime)?;
        }
        Ok(w.finish())
    }

    /// The commands that make one inode at `path`: the create, then its
    /// data, its extended attributes and its size.
    fn send_create(
        &self,
        tree: &Filesystem,
        w: &mut StreamWriter,
        path: &[u8],
        inode: &Inode,
        opts: SendOptions,
    ) -> Result<()> {
        let kind = inode.file_type().ok_or_else(|| {
            Error::UnsupportedFeature(format!(
                "inode {} has mode {:o}, which is no file type",
                inode.ino, inode.mode
            ))
        })?;
        let command = match kind {
            FileType::Regular => cmd::MKFILE,
            FileType::Directory => cmd::MKDIR,
            FileType::Symlink => cmd::SYMLINK,
            FileType::CharDevice | FileType::BlockDevice => cmd::MKNOD,
            FileType::Fifo => cmd::MKFIFO,
            FileType::Socket => cmd::MKSOCK,
        };
        w.begin(command);
        w.attr(attr::PATH, path)?;
        w.attr_u64(attr::INO, inode.ino)?;
        match kind {
            FileType::Symlink => w.attr(attr::PATH_LINK, &tree.read_link(inode.ino)?)?,
            FileType::CharDevice | FileType::BlockDevice | FileType::Fifo | FileType::Socket => {
                w.attr_u64(attr::MODE, u64::from(inode.mode))?;
                w.attr_u64(attr::RDEV, stream_rdev(inode.rdev))?;
            }
            _ => {}
        }

        if kind == FileType::Regular {
            self.send_data(tree, w, path, inode, opts)?;
        }
        for x in tree.list_xattrs(inode.ino)? {
            w.begin(cmd::SET_XATTR);
            w.attr(attr::PATH, path)?;
            w.attr(attr::XATTR_NAME, &x.name)?;
            w.attr(attr::XATTR_DATA, &x.value)?;
        }
        if kind == FileType::Regular {
            w.begin(cmd::TRUNCATE);
            w.attr(attr::PATH, path)?;
            w.attr_u64(attr::SIZE, inode.size)?;
        }
        Ok(())
    }

    /// A file's data, extent item by extent item.
    ///
    /// Bytes go as `WRITE` commands, one per [`SEND_WRITE_CHUNK`] at most.
    /// Holes are left out: they read as zeros, and the closing `TRUNCATE`
    /// gives the file its size. A preallocated range is left out of a
    /// version-1 stream for the same reason, and kept as a `FALLOCATE` in a
    /// version-2 one. A compressed extent goes as an `ENCODED_WRITE` when
    /// [`SendOptions::compressed_data`] asks and the stream can name its
    /// compression; otherwise its bytes are decoded and written.
    fn send_data(
        &self,
        tree: &Filesystem,
        w: &mut StreamWriter,
        path: &[u8],
        inode: &Inode,
        opts: SendOptions,
    ) -> Result<()> {
        for ((objectid, key_type, start), item) in tree.item_run(inode.ino, EXTENT_DATA_KEY)? {
            if objectid != inode.ino || key_type != EXTENT_DATA_KEY {
                break;
            }
            let x = RawExtent::parse(&item, inode.ino, start)?;
            match x.kind {
                EXTENT_INLINE => send_writes(
                    tree,
                    w,
                    path,
                    inode,
                    start,
                    start.saturating_add(x.ram_bytes),
                )?,
                EXTENT_PREALLOC if opts.version >= 2 => {
                    w.begin(cmd::FALLOCATE);
                    w.attr(attr::PATH, path)?;
                    w.attr_u32(attr::FALLOCATE_MODE, FALLOC_FL_KEEP_SIZE)?;
                    w.attr_u64(attr::FILE_OFFSET, start)?;
                    w.attr_u64(attr::SIZE, x.num_bytes)?;
                }
                EXTENT_PREALLOC => {}
                _ if x.disk_bytenr == 0 => {} // a hole
                _ => {
                    let end = start.saturating_add(x.num_bytes);
                    if !(opts.compressed_data && send_encoded(tree, w, path, inode, start, &x)?) {
                        send_writes(tree, w, path, inode, start, end)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// The subvolume's name, UUID, change transaction and root, from the
    /// root tree; refused unless it is read-only.
    fn send_identity(&self, id: u64) -> Result<SubvolIdentity> {
        if !is_subvolume_id(id) {
            return Err(Error::NotFound);
        }
        let subvol = self
            .subvolumes()?
            .into_iter()
            .find(|s| s.id == id)
            .ok_or(Error::NotFound)?;
        let item = self
            .root_tree_items()?
            .into_iter()
            .find(|(objectid, key_type, _, _)| *objectid == id && *key_type == ROOT_ITEM_KEY)
            .map(|(_, _, _, data)| data)
            .ok_or(Error::NotFound)?;
        if item.len() < ROOT_ITEM_CTRANSID + 8 {
            return Err(Error::UnsupportedFeature(format!(
                "subvolume {id}'s root item is {} bytes and predates the UUID fields a \
                 send stream names it by",
                item.len()
            )));
        }
        let flags = u64::from_le_bytes(
            item[root_item::FLAGS..root_item::FLAGS + 8]
                .try_into()
                .expect("8 bytes"),
        );
        if flags & ROOT_SUBVOL_RDONLY == 0 {
            return Err(Error::UnsupportedFeature(format!(
                "subvolume {id} is not read-only, and a send stream describes one state of \
                 a subvolume that cannot change"
            )));
        }
        Ok(SubvolIdentity {
            name: subvol.name,
            uuid: item[ROOT_ITEM_UUID..ROOT_ITEM_UUID + 16]
                .try_into()
                .expect("16 bytes"),
            ctransid: u64::from_le_bytes(
                item[ROOT_ITEM_CTRANSID..ROOT_ITEM_CTRANSID + 8]
                    .try_into()
                    .expect("8 bytes"),
            ),
            bytenr: subvol.bytenr,
        })
    }
}

/// A file extent item's type byte: inline, regular, preallocated.
const EXTENT_INLINE: u8 = 0;
const EXTENT_PREALLOC: u8 = 2;

/// The most a compressed extent holds on disk, and so the most one
/// `ENCODED_WRITE` carries.
const MAX_COMPRESSED: u64 = 128 * 1024;

/// One file extent item, its fields as stored.
struct RawExtent {
    kind: u8,
    compression: u8,
    encryption: u8,
    other_encoding: u16,
    ram_bytes: u64,
    disk_bytenr: u64,
    disk_num_bytes: u64,
    offset: u64,
    num_bytes: u64,
}

impl RawExtent {
    fn parse(item: &[u8], ino: u64, start: u64) -> Result<Self> {
        let need = if item.get(file_extent::TYPE) == Some(&EXTENT_INLINE) {
            file_extent::INLINE_DATA
        } else {
            file_extent::REGULAR_SIZE
        };
        if item.len() < need {
            return Err(Error::BadSuperblock(format!(
                "inode {ino}: the extent item at {start} is {} bytes, {need} needed",
                item.len()
            )));
        }
        let regular = |at: usize| {
            if need == file_extent::REGULAR_SIZE {
                le64(item, at)
            } else {
                0
            }
        };
        Ok(Self {
            kind: item[file_extent::TYPE],
            compression: item[file_extent::COMPRESSION],
            encryption: item[file_extent::ENCRYPTION],
            other_encoding: u16::from_le_bytes(
                item[file_extent::OTHER_ENCODING..file_extent::OTHER_ENCODING + 2]
                    .try_into()
                    .expect("2 bytes"),
            ),
            ram_bytes: le64(item, file_extent::RAM_BYTES),
            disk_bytenr: regular(file_extent::DISK_BYTENR),
            disk_num_bytes: regular(file_extent::DISK_NUM_BYTES),
            offset: regular(file_extent::OFFSET),
            num_bytes: regular(file_extent::NUM_BYTES),
        })
    }
}

/// The stream's number for an extent's compression, for a volume of
/// `sectorsize`; `None` for none, or one the stream cannot name.
fn encoded_compression(on_disk: u8, sectorsize: u32) -> Option<u32> {
    match Compression::from_byte(on_disk).ok()? {
        Compression::None => None,
        Compression::Zlib => Some(encoded::ZLIB),
        Compression::Zstd => Some(encoded::ZSTD),
        Compression::Lzo => match sectorsize {
            4096 => Some(encoded::LZO_4K),
            8192 => Some(encoded::LZO_4K + 1),
            16384 => Some(encoded::LZO_4K + 2),
            32768 => Some(encoded::LZO_4K + 3),
            65536 => Some(encoded::LZO_4K + 4),
            _ => None,
        },
    }
}

/// `WRITE`s of the file's bytes from `start` to `end`, cut at its size.
fn send_writes(
    tree: &Filesystem,
    w: &mut StreamWriter,
    path: &[u8],
    inode: &Inode,
    start: u64,
    end: u64,
) -> Result<()> {
    let end = end.min(inode.size);
    let mut pos = start;
    while pos < end {
        let n = (end - pos).min(SEND_WRITE_CHUNK as u64) as usize;
        let mut buf = vec![0u8; n];
        let got = tree.read_at(inode.ino, pos, &mut buf)?;
        buf.truncate(got);
        if buf.is_empty() {
            break;
        }
        w.begin(cmd::WRITE);
        w.attr(attr::PATH, path)?;
        w.attr_u64(attr::FILE_OFFSET, pos)?;
        w.attr_data(&buf)?;
        pos += buf.len() as u64;
    }
    Ok(())
}

/// A compressed extent as one `ENCODED_WRITE` of its bytes as they are on
/// disk, filling the file from `start` to the extent's end or the file's
/// size, whichever comes first. Whether it was sent: an extent the stream
/// cannot describe is left to [`send_writes`].
fn send_encoded(
    tree: &Filesystem,
    w: &mut StreamWriter,
    path: &[u8],
    inode: &Inode,
    start: u64,
    x: &RawExtent,
) -> Result<bool> {
    let Some(compression) = encoded_compression(x.compression, tree.sb.sectorsize) else {
        return Ok(false);
    };
    let file_len = x.num_bytes.min(inode.size.saturating_sub(start));
    if x.encryption != 0
        || x.other_encoding != 0
        || file_len == 0
        || x.disk_num_bytes == 0
        || x.disk_num_bytes > MAX_COMPRESSED
    {
        return Ok(false);
    }
    let mut packed = vec![0u8; x.disk_num_bytes as usize];
    tree.read_data_verified(x.disk_bytenr, &mut packed, true)?;
    w.begin(cmd::ENCODED_WRITE);
    w.attr(attr::PATH, path)?;
    w.attr_u64(attr::FILE_OFFSET, start)?;
    w.attr_u64(attr::UNENCODED_FILE_LEN, file_len)?;
    w.attr_u64(attr::UNENCODED_LEN, x.ram_bytes)?;
    w.attr_u64(attr::UNENCODED_OFFSET, x.offset)?;
    w.attr_u32(attr::COMPRESSION, compression)?;
    w.attr_u32(attr::ENCRYPTION, 0)?;
    w.attr_data(&packed)?;
    Ok(true)
}

/// An inode item's device number as a send stream carries it.
///
/// The inode item packs the major number above bit 20 and the minor below
/// it; the stream carries the number `mknod(2)` takes, whose low byte is the
/// minor's low byte, bits 8..20 the major and bits 20 up the rest of the
/// minor. Observed against the kernel: a `c 1 3` node is `0x100003` on disk
/// and `0x103` in the stream `btrfs send` writes for it.
fn stream_rdev(on_disk: u64) -> u64 {
    let major = on_disk >> 20;
    let minor = on_disk & 0xf_ffff;
    (minor & 0xff) | ((major & 0xfff) << 8) | ((minor & !0xff) << 12) | ((major & !0xfff) << 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The device number is re-packed, not copied: `/dev/null` (1:3) is
    /// `0x100003` in the inode item and `0x103` in the stream.
    #[test]
    fn a_device_number_is_repacked_for_the_stream() {
        assert_eq!(stream_rdev(0x0010_0003), 0x103);
        assert_eq!(stream_rdev(0), 0);
        // 8:300 -- a minor above 255 moves its high bits above the major.
        assert_eq!(stream_rdev((8 << 20) | 300), 0x0010_082c);
    }

    /// A stream built by the writer reads back command for command.
    #[test]
    fn a_written_stream_parses_back() {
        let mut w = StreamWriter::new();
        w.begin(cmd::SUBVOL);
        w.attr(attr::PATH, b"snap").unwrap();
        w.attr(attr::UUID, &[7; 16]).unwrap();
        w.attr_u64(attr::CTRANSID, 42).unwrap();
        w.begin(cmd::WRITE);
        w.attr(attr::PATH, b"a").unwrap();
        w.attr_u64(attr::FILE_OFFSET, 4096).unwrap();
        w.attr(attr::DATA, b"hello").unwrap();
        let bytes = w.finish();

        let s = parse_send_stream(&bytes).unwrap();
        assert_eq!(s.version, 1);
        assert_eq!(s.commands.len(), 3);
        assert_eq!(s.commands[0].cmd, cmd::SUBVOL);
        assert_eq!(s.commands[0].path().unwrap(), b"snap");
        assert_eq!(s.commands[0].u64(attr::CTRANSID).unwrap(), 42);
        assert_eq!(s.commands[1].u64(attr::FILE_OFFSET).unwrap(), 4096);
        assert_eq!(s.commands[1].attr(attr::DATA).unwrap(), b"hello");
        assert_eq!(s.commands[2].cmd, cmd::END);
    }

    /// A flipped byte anywhere in a command fails its checksum.
    #[test]
    fn a_damaged_command_fails_its_checksum() {
        let mut w = StreamWriter::new();
        w.begin(cmd::MKFILE);
        w.attr(attr::PATH, b"file").unwrap();
        let mut bytes = w.finish();
        let last_path_byte = 17 + COMMAND_HEADER_LEN + ATTR_HEADER_LEN + 3;
        bytes[last_path_byte] ^= 1;
        assert!(matches!(
            parse_send_stream(&bytes),
            Err(StreamError::ChecksumMismatch { offset: 17 })
        ));
    }

    /// The stream checksum is the raw register: seed zero, no final
    /// inversion. Of nothing it is zero; the conventional CRC-32C of
    /// nothing is also zero, so a byte is what tells them apart.
    #[test]
    fn the_stream_crc_is_not_the_conventional_crc32c() {
        assert_eq!(stream_crc(b""), 0);
        assert_ne!(stream_crc(b"a"), crc32c::crc32c(b"a"));
    }

    #[test]
    fn a_stream_without_an_end_is_refused() {
        let mut bytes = StreamWriter::new().finish();
        bytes.truncate(17);
        assert!(matches!(
            parse_send_stream(&bytes),
            Err(StreamError::Malformed(_))
        ));
    }

    #[test]
    fn bytes_after_the_end_are_refused() {
        let mut bytes = StreamWriter::new().finish();
        bytes.push(0);
        assert!(matches!(
            parse_send_stream(&bytes),
            Err(StreamError::Malformed(_))
        ));
    }

    #[test]
    fn a_wrong_magic_or_version_is_refused() {
        let mut bytes = StreamWriter::new().finish();
        bytes[0] = b'x';
        assert!(matches!(
            parse_send_stream(&bytes),
            Err(StreamError::Malformed(_))
        ));
        let mut bytes = StreamWriter::new().finish();
        bytes[13] = 9;
        assert!(matches!(
            parse_send_stream(&bytes),
            Err(StreamError::UnsupportedVersion(9))
        ));
    }

    #[test]
    fn an_attribute_past_its_command_is_refused() {
        let mut w = StreamWriter::new();
        w.begin(cmd::MKFILE);
        w.attr(attr::PATH, b"file").unwrap();
        let mut bytes = w.finish();
        // The attribute's length: 4 becomes 40.
        bytes[17 + COMMAND_HEADER_LEN + 2] = 40;
        // Re-stamp the checksum so the length is what is judged.
        bytes[17 + 6..17 + 10].fill(0);
        let crc = stream_crc(&bytes[17..17 + COMMAND_HEADER_LEN + ATTR_HEADER_LEN + 4]);
        bytes[17 + 6..17 + 10].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            parse_send_stream(&bytes),
            Err(StreamError::Malformed(_))
        ));
    }

    /// Version 2's DATA runs to the end of its command, with no length.
    #[test]
    fn version_two_data_runs_to_the_end_of_its_command() {
        let mut body = Vec::new();
        body.extend_from_slice(&attr::PATH.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes());
        body.push(b'a');
        body.extend_from_slice(&attr::DATA.to_le_bytes());
        body.extend_from_slice(b"no length here");
        let mut stream = SEND_STREAM_MAGIC.to_vec();
        stream.extend_from_slice(&2u32.to_le_bytes());
        for (cmd, body) in [(cmd::WRITE, body), (cmd::END, Vec::new())] {
            let at = stream.len();
            stream.extend_from_slice(&(body.len() as u32).to_le_bytes());
            stream.extend_from_slice(&cmd.to_le_bytes());
            stream.extend_from_slice(&[0; 4]);
            stream.extend_from_slice(&body);
            let crc = stream_crc(&stream[at..]);
            stream[at + 6..at + 10].copy_from_slice(&crc.to_le_bytes());
        }
        let s = parse_send_stream(&stream).unwrap();
        assert_eq!(s.version, 2);
        assert_eq!(s.commands[0].attr(attr::DATA).unwrap(), b"no length here");
    }

    /// A version-2 writer leaves `DATA` unmeasured, so a write longer than
    /// a `u16` counts goes in one command and parses back whole.
    #[test]
    fn a_version_two_write_carries_more_than_a_u16_counts() {
        let big: Vec<u8> = (0..100_000u32).map(|i| i as u8).collect();
        let mut w = StreamWriter::with_version(2);
        w.begin(cmd::WRITE);
        w.attr(attr::PATH, b"f").unwrap();
        w.attr_u64(attr::FILE_OFFSET, 0).unwrap();
        w.attr_data(&big).unwrap();
        let s = parse_send_stream(&w.finish()).unwrap();
        assert_eq!(s.version, 2);
        assert_eq!(s.commands[0].attr(attr::DATA).unwrap(), &big[..]);

        let mut w = StreamWriter::new();
        w.begin(cmd::WRITE);
        assert!(w.attr_data(&big).is_err(), "version 1 cannot carry it");
    }

    /// The stream numbers compression its own way, and splits LZO by the
    /// sector size.
    #[test]
    fn an_encoded_write_names_its_compression_as_the_stream_does() {
        assert_eq!(encoded_compression(1, 4096), Some(encoded::ZLIB));
        assert_eq!(encoded_compression(3, 4096), Some(encoded::ZSTD));
        assert_eq!(encoded_compression(2, 4096), Some(encoded::LZO_4K));
        assert_eq!(encoded_compression(2, 65536), Some(encoded::LZO_4K + 4));
        assert_eq!(encoded_compression(0, 4096), None);
        assert_eq!(encoded_compression(9, 4096), None);
    }

    /// Options no stream can carry are refused before anything is read.
    #[test]
    fn options_no_stream_carries_are_refused() {
        assert!(SendOptions::v1().check().is_ok());
        assert!(SendOptions::v2().with_compressed_data(true).check().is_ok());
        assert!(SendOptions::v1()
            .with_compressed_data(true)
            .check()
            .is_err());
        let mut three = SendOptions::v2();
        three.version = 3;
        assert!(three.check().is_err());
    }

    #[test]
    fn every_command_up_to_verity_has_a_name() {
        for c in 1..=26 {
            assert!(command_name(c).is_some(), "command {c}");
        }
        assert_eq!(command_name(0), None);
        assert_eq!(command_name(27), None);
    }
}
