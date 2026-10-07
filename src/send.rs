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
//! a `u16` can count. Both versions are read here. Only version 1 is
//! written, and version 2's new commands (preallocate, inode flags,
//! encoded writes) are carried through the parser as raw attributes.
//!
//! # What a written stream holds
//!
//! A full stream: no parent snapshot, so every inode is created. The
//! kernel creates each inode under a temporary name and renames it into
//! place; a receiver only needs each command to make sense when it is
//! applied, so here every inode is created at its final path, parents
//! before children. Then, per inode, its data (holes and preallocated
//! ranges are left unwritten, as the kernel leaves them), its extended
//! attributes and its size; owners and modes once everything exists; and
//! the times last and deepest first, because creating an entry moves its
//! directory's modification time.
//!
//! The oracle is the other implementation: `tests/send_stream_kernel.rs`
//! parses streams the kernel's `btrfs send` wrote and has the guest's
//! `btrfs receive` replay streams this module wrote.

use std::collections::{BTreeMap, VecDeque};

use crate::error::{Error, Result};
use crate::fs::{root_item, Filesystem, ROOT_ITEM_KEY};
use crate::inode::{FileType, Inode, Timestamp};
use crate::subvol::is_subvolume_id;

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
}

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
    /// [`Error::BadSendStream`] when it is absent.
    pub fn require(&self, ty: u16) -> Result<&[u8]> {
        self.attr(ty).ok_or_else(|| {
            Error::BadSendStream(format!(
                "a {} command has no attribute {ty}",
                command_name(self.cmd).unwrap_or("unknown")
            ))
        })
    }

    /// A `u64` attribute.
    ///
    /// # Errors
    ///
    /// [`Error::BadSendStream`] when it is absent or not eight bytes.
    pub fn u64(&self, ty: u16) -> Result<u64> {
        let v = self.require(ty)?;
        let bytes: [u8; 8] = v.try_into().map_err(|_| {
            Error::BadSendStream(format!(
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
    /// [`Error::BadSendStream`] when it is absent or not twelve bytes.
    pub fn timestamp(&self, ty: u16) -> Result<Timestamp> {
        let v = self.require(ty)?;
        if v.len() != 12 {
            return Err(Error::BadSendStream(format!(
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
    /// [`Error::BadSendStream`] when it is absent.
    pub fn path(&self) -> Result<&[u8]> {
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
/// [`Error::BadSendStream`] naming the offset of what is wrong,
/// [`Error::ChecksumMismatch`] for a command whose checksum disagrees,
/// and [`Error::UnsupportedFeature`] for a version this does not read.
pub fn parse_send_stream(bytes: &[u8]) -> Result<SendStream> {
    let header_len = SEND_STREAM_MAGIC.len() + 4;
    if bytes.len() < header_len || &bytes[..SEND_STREAM_MAGIC.len()] != SEND_STREAM_MAGIC {
        return Err(Error::BadSendStream(
            "the stream does not open with \"btrfs-stream\\0\"".into(),
        ));
    }
    let version = u32::from_le_bytes(
        bytes[SEND_STREAM_MAGIC.len()..header_len]
            .try_into()
            .expect("4 bytes"),
    );
    if version != 1 && version != 2 {
        return Err(Error::UnsupportedFeature(format!(
            "send stream version {version}; versions 1 and 2 are read"
        )));
    }

    let mut commands = Vec::new();
    let mut at = header_len;
    loop {
        if at == bytes.len() {
            return Err(Error::BadSendStream(format!(
                "the stream stops at byte {at} without an end command"
            )));
        }
        if bytes.len() - at < COMMAND_HEADER_LEN {
            return Err(Error::BadSendStream(format!(
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
                Error::BadSendStream(format!(
                    "the command at byte {at} claims {len} bytes, past the end of the stream"
                ))
            })?;

        let mut framed = bytes[at..end].to_vec();
        framed[6..10].fill(0);
        if stream_crc(&framed) != crc {
            return Err(Error::ChecksumMismatch {
                what: "send stream command",
                offset: at as u64,
            });
        }

        let attrs = parse_attrs(&bytes[body_start..end], body_start, version)?;
        commands.push(Command { cmd, attrs });
        at = end;
        if cmd == cmd::END {
            break;
        }
    }
    if at != bytes.len() {
        return Err(Error::BadSendStream(format!(
            "{} bytes follow the end command at byte {at}",
            bytes.len() - at
        )));
    }
    Ok(SendStream { version, commands })
}

/// The attributes of one command. `base` is where `body` starts in the
/// stream, for the error messages.
fn parse_attrs(body: &[u8], base: usize, version: u32) -> Result<Vec<(u16, Vec<u8>)>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < body.len() {
        if body.len() - at < 2 {
            return Err(Error::BadSendStream(format!(
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
            return Err(Error::BadSendStream(format!(
                "an attribute header at byte {} is cut short",
                base + at
            )));
        }
        let len = u16::from_le_bytes(body[at + 2..at + 4].try_into().expect("2 bytes")) as usize;
        let start = at + ATTR_HEADER_LEN;
        let end = start + len;
        if end > body.len() {
            return Err(Error::BadSendStream(format!(
                "attribute {ty} at byte {} claims {len} bytes, past the end of its command",
                base + at
            )));
        }
        out.push((ty, body[start..end].to_vec()));
        at = end;
    }
    Ok(out)
}

/// Builds a version-1 stream, one command at a time.
#[derive(Debug)]
pub struct StreamWriter {
    out: Vec<u8>,
    cmd: Option<(usize, u16)>,
}

impl Default for StreamWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamWriter {
    /// A stream holding only its header.
    pub fn new() -> Self {
        let mut out = SEND_STREAM_MAGIC.to_vec();
        out.extend_from_slice(&1u32.to_le_bytes());
        Self { out, cmd: None }
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
    /// [`Error::BadSendStream`] for a value longer than a `u16` counts.
    pub fn attr(&mut self, ty: u16, value: &[u8]) -> Result<()> {
        let len = u16::try_from(value.len()).map_err(|_| {
            Error::BadSendStream(format!(
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
    pub fn attr_u64(&mut self, ty: u16, value: u64) -> Result<()> {
        self.attr(ty, &value.to_le_bytes())
    }

    /// Add a timestamp attribute.
    pub fn attr_time(&mut self, ty: u16, t: Timestamp) -> Result<()> {
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
        let subvol = self.send_identity(id)?;
        let tree = self.open_subvolume_at(subvol.bytenr)?;

        let mut w = StreamWriter::new();
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
                self.send_create(&tree, &mut w, &path, &inode)?;
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
                w.attr_u64(attr::RDEV, inode.rdev)?;
            }
            _ => {}
        }

        if kind == FileType::Regular {
            self.send_data(tree, w, path, inode)?;
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

    /// A file's bytes as `WRITE` commands, one per [`SEND_WRITE_CHUNK`]
    /// at most. Holes and preallocated ranges are left out: both read as
    /// zeros, and the closing `TRUNCATE` gives the file its size.
    fn send_data(
        &self,
        tree: &Filesystem,
        w: &mut StreamWriter,
        path: &[u8],
        inode: &Inode,
    ) -> Result<()> {
        for piece in tree.file_extents(inode.ino)? {
            // Preallocated: no address, and an extent behind it.
            if piece.logical.is_none() && piece.extent_start != 0 && !piece.compressed {
                continue;
            }
            let end = piece.start.saturating_add(piece.len).min(inode.size);
            let mut pos = piece.start;
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
                w.attr(attr::DATA, &buf)?;
                pos += buf.len() as u64;
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

#[cfg(test)]
mod tests {
    use super::*;

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
            Err(Error::ChecksumMismatch { offset: 17, .. })
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
            Err(Error::BadSendStream(_))
        ));
    }

    #[test]
    fn bytes_after_the_end_are_refused() {
        let mut bytes = StreamWriter::new().finish();
        bytes.push(0);
        assert!(matches!(
            parse_send_stream(&bytes),
            Err(Error::BadSendStream(_))
        ));
    }

    #[test]
    fn a_wrong_magic_or_version_is_refused() {
        let mut bytes = StreamWriter::new().finish();
        bytes[0] = b'x';
        assert!(matches!(
            parse_send_stream(&bytes),
            Err(Error::BadSendStream(_))
        ));
        let mut bytes = StreamWriter::new().finish();
        bytes[13] = 9;
        assert!(matches!(
            parse_send_stream(&bytes),
            Err(Error::UnsupportedFeature(_))
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
            Err(Error::BadSendStream(_))
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

    #[test]
    fn every_command_up_to_verity_has_a_name() {
        for c in 1..=26 {
            assert!(command_name(c).is_some(), "command {c}");
        }
        assert_eq!(command_name(0), None);
        assert_eq!(command_name(27), None);
    }
}
