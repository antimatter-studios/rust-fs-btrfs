//! fs-verity: a file sealed read-only, every block of it checked against
//! a Merkle tree whose root hash is part of the file's digest (#270).
//!
//! A volume carries the `verity` compat_ro bit once any of its files has
//! had verity enabled. Such a file is marked by `BTRFS_INODE_RO_VERITY`,
//! bit 0 of the inode's read-only flags, which btrfs keeps in the upper
//! half of the 64-bit `flags` field. Its verity metadata lives in the fs
//! tree beside its inode, under two key types of its own:
//!
//! ```text
//! (ino, VERITY_DESC_ITEM 36, 0)       btrfs_verity_descriptor_item:
//!                                       le64 size, le64 reserved[2],
//!                                       u8 encryption
//! (ino, VERITY_DESC_ITEM 36, 1 + n)   the fs-verity descriptor, from its
//!                                       byte n on: `size` bytes in all
//! (ino, VERITY_MERKLE_ITEM 37, n)     the Merkle tree, from its byte n on
//! ```
//!
//! Both blobs are cut into items of at most 2 KiB as the kernel writes
//! them, so each is read as the run of items under its key type, each
//! item's bytes placed at its key offset.
//!
//! # What the descriptor says, and what is checked
//!
//! The descriptor is the generic fs-verity one, 256 bytes and an optional
//! signature: a version (1), the hash (1 SHA-256, 2 SHA-512), the log2 of
//! the block size, the salt, the size of the data the tree covers, and the
//! tree's root hash. The file's **digest** -- what `fsverity measure`
//! prints, and what a signature signs -- is the hash of those 256 bytes
//! with the signature size zeroed.
//!
//! The tree hashes each block of the file, the last one zero-padded, as
//! `H(salt padded to the hash's block size || block)` (no salt, no
//! prefix). Those digests, packed into blocks and the last one
//! zero-padded, are level 0; level 0's blocks are hashed the same way
//! into level 1, and so on until a level is one block, whose hash is the
//! root. The tree stores the levels **root first**: the top level, then
//! the one below it, down to level 0. A one-block file has no tree, its
//! root hash is that block's; an empty file's root hash is all zeros.
//!
//! A read of a verity file is checked the way the kernel checks it: each
//! block read is hashed, its digest compared with its entry in level 0,
//! that level-0 block's digest with its entry a level up, and so on to the
//! root hash in the descriptor. A block that does not verify is refused,
//! never returned -- the kernel's answer is `EIO`, and so is this one's
//! through the C ABI. Only the tree blocks a read needs are fetched.

use crate::error::{Error, Result};

/// `BTRFS_VERITY_DESC_ITEM_KEY`.
pub const VERITY_DESC_ITEM_KEY: u8 = 36;
/// `BTRFS_VERITY_MERKLE_ITEM_KEY`.
pub const VERITY_MERKLE_ITEM_KEY: u8 = 37;

/// `BTRFS_INODE_RO_VERITY`, as it sits in the 64-bit `flags` field: bit 0
/// of the read-only flags, which are the upper 32 bits.
pub const INODE_RO_VERITY: u64 = 1 << 32;

/// Refuse a data mutation or removal before any bytes are written. A
/// sealed inode keeps its descriptor and Merkle tree unchanged (#452).
pub(crate) fn refuse_mutation(ino: u64, flags: u64) -> Result<()> {
    if flags & INODE_RO_VERITY != 0 {
        return Err(Error::UnsupportedFeature(format!(
            "inode {ino} is sealed by fs-verity, so its data and verity items cannot be changed"
        )));
    }
    Ok(())
}

/// Size of `struct btrfs_verity_descriptor_item`: `size`, two reserved
/// `u64`s and the `encryption` byte, packed without trailing padding.
/// The kernel-made oracle fixture stores all six headers as 25 bytes.
pub const DESCRIPTOR_ITEM_SIZE: usize = 25;

/// Size of `struct fsverity_descriptor` without its signature.
pub const DESCRIPTOR_SIZE: usize = 256;

/// The largest salt a descriptor holds.
pub const MAX_SALT: usize = 32;

/// Byte offsets within `struct fsverity_descriptor`.
mod desc {
    pub const VERSION: usize = 0;
    pub const HASH_ALGORITHM: usize = 1;
    pub const LOG_BLOCKSIZE: usize = 2;
    pub const SALT_SIZE: usize = 3;
    /// `le32`. Zeroed before the descriptor is hashed into the digest.
    pub const SIG_SIZE: usize = 4;
    pub const DATA_SIZE: usize = 8;
    /// 64 bytes, of which the hash's own length is used.
    pub const ROOT_HASH: usize = 16;
    /// 32 bytes, of which `salt_size` are used.
    pub const SALT: usize = 80;
    /// 144 bytes, all zero.
    pub const RESERVED: usize = 112;
}

/// The two hashes fs-verity defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerityHash {
    /// `FS_VERITY_HASH_ALG_SHA256`, 1.
    Sha256,
    /// `FS_VERITY_HASH_ALG_SHA512`, 2.
    Sha512,
}

impl VerityHash {
    fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::Sha256),
            2 => Some(Self::Sha512),
            _ => None,
        }
    }

    /// The digest length in bytes.
    pub fn digest_len(self) -> usize {
        match self {
            Self::Sha256 => 32,
            Self::Sha512 => 64,
        }
    }

    /// The hash's own input block size, which the salt is padded to.
    fn input_block(self) -> usize {
        match self {
            Self::Sha256 => 64,
            Self::Sha512 => 128,
        }
    }

    /// The name `fsverity measure` prints before the digest.
    pub fn name(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
        }
    }

    fn hash(self, parts: &[&[u8]]) -> Vec<u8> {
        use sha2::Digest;
        match self {
            Self::Sha256 => {
                let mut h = sha2::Sha256::new();
                for p in parts {
                    h.update(p);
                }
                h.finalize().to_vec()
            }
            Self::Sha512 => {
                let mut h = sha2::Sha512::new();
                for p in parts {
                    h.update(p);
                }
                h.finalize().to_vec()
            }
        }
    }
}

/// A verity file's descriptor, parsed and checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verity {
    /// The Merkle tree's hash.
    pub hash: VerityHash,
    /// log2 of the data and tree block size.
    pub log_blocksize: u8,
    /// The size of the data the tree covers; the file's size.
    pub data_size: u64,
    /// The salt, as long as the descriptor says.
    pub salt: Vec<u8>,
    /// The tree's root hash, as long as the hash's digest.
    pub root_hash: Vec<u8>,
    /// The signature after the descriptor, empty when there is none.
    pub signature: Vec<u8>,
    /// The descriptor exactly as stored, signature included: what
    /// `fsverity dump_metadata descriptor` prints.
    pub descriptor: Vec<u8>,
}

impl Verity {
    /// Parse and check a stored descriptor (and its signature) for a file
    /// of `file_size` bytes.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedFeature`] for a version, hash or block size
    /// fs-verity does not define, and [`Error::BadSuperblock`] for a
    /// descriptor that contradicts itself or the file: a size that is not
    /// the file's, a salt longer than its field, reserved bytes set, or a
    /// signature size that is not what follows the descriptor.
    pub fn parse(descriptor: Vec<u8>, file_size: u64) -> Result<Self> {
        let bad = |m: String| Error::BadSuperblock(format!("fs-verity descriptor: {m}"));
        if descriptor.len() < DESCRIPTOR_SIZE {
            return Err(bad(format!(
                "{} bytes, shorter than the {DESCRIPTOR_SIZE} the structure needs",
                descriptor.len()
            )));
        }
        let d = &descriptor;
        if d[desc::VERSION] != 1 {
            return Err(Error::UnsupportedFeature(format!(
                "fs-verity descriptor version {}",
                d[desc::VERSION]
            )));
        }
        let hash = VerityHash::from_raw(d[desc::HASH_ALGORITHM]).ok_or_else(|| {
            Error::UnsupportedFeature(format!(
                "fs-verity hash algorithm {}",
                d[desc::HASH_ALGORITHM]
            ))
        })?;
        let log_blocksize = d[desc::LOG_BLOCKSIZE];
        // 1 KiB to 64 KiB: the kernel's own bounds are 1 KiB and the page
        // size, and a block must hold at least two digests for the tree
        // to narrow towards a root.
        if !(10..=16).contains(&log_blocksize) {
            return Err(Error::UnsupportedFeature(format!(
                "an fs-verity block size of 2^{log_blocksize} bytes"
            )));
        }
        let salt_size = usize::from(d[desc::SALT_SIZE]);
        if salt_size > MAX_SALT {
            return Err(bad(format!("a salt of {salt_size} bytes")));
        }
        let sig_size =
            u32::from_le_bytes(d[desc::SIG_SIZE..desc::SIG_SIZE + 4].try_into().unwrap());
        if DESCRIPTOR_SIZE as u64 + u64::from(sig_size) != d.len() as u64 {
            return Err(bad(format!(
                "a {sig_size}-byte signature, but {} bytes follow the descriptor",
                d.len() - DESCRIPTOR_SIZE
            )));
        }
        let data_size =
            u64::from_le_bytes(d[desc::DATA_SIZE..desc::DATA_SIZE + 8].try_into().unwrap());
        if data_size != file_size {
            return Err(bad(format!(
                "it covers {data_size} bytes of a {file_size}-byte file"
            )));
        }
        if d[desc::RESERVED..DESCRIPTOR_SIZE].iter().any(|&b| b != 0) {
            return Err(bad("reserved bytes are set".into()));
        }
        let root_hash = d[desc::ROOT_HASH..desc::ROOT_HASH + hash.digest_len()].to_vec();
        let salt = d[desc::SALT..desc::SALT + salt_size].to_vec();
        let signature = d[DESCRIPTOR_SIZE..].to_vec();
        Ok(Self {
            hash,
            log_blocksize,
            data_size,
            salt,
            root_hash,
            signature,
            descriptor,
        })
    }

    /// The data and tree block size.
    pub fn block_size(&self) -> u64 {
        1u64 << self.log_blocksize
    }

    /// The file's digest: the hash of the descriptor with its signature
    /// size zeroed and its signature left off. What `fsverity measure`
    /// reports, and what a signature signs.
    pub fn digest(&self) -> Vec<u8> {
        let mut d = self.descriptor[..DESCRIPTOR_SIZE].to_vec();
        d[desc::SIG_SIZE..desc::SIG_SIZE + 4].fill(0);
        self.hash.hash(&[&d])
    }

    /// The digest as `fsverity measure` prints it: `sha256:` and hex.
    pub fn measurement(&self) -> String {
        let hex: String = self.digest().iter().map(|b| format!("{b:02x}")).collect();
        format!("{}:{hex}", self.hash.name())
    }

    fn hashes_per_block(&self) -> u64 {
        self.block_size() / self.hash.digest_len() as u64
    }

    /// `(first block, blocks)` of each level within the stored tree, level
    /// 0 first. Empty for a file of one block or none.
    fn levels(&self) -> Vec<(u64, u64)> {
        let per = self.hashes_per_block();
        let mut blocks = self.data_size.div_ceil(self.block_size());
        let mut sizes = Vec::new();
        while blocks > 1 {
            blocks = blocks.div_ceil(per);
            sizes.push(blocks);
        }
        // Stored root first: the top level at block 0.
        let mut starts = vec![0u64; sizes.len()];
        let mut at = 0u64;
        for level in (0..sizes.len()).rev() {
            starts[level] = at;
            at += sizes[level];
        }
        starts.into_iter().zip(sizes).collect()
    }

    /// The stored tree's size in bytes.
    pub fn tree_size(&self) -> u64 {
        self.levels().iter().map(|(_, n)| n).sum::<u64>() * self.block_size()
    }

    /// `H(padded salt || block)`, the block zero-padded to the block size.
    fn hash_block(&self, block: &[u8]) -> Vec<u8> {
        let bs = self.block_size() as usize;
        let mut salt = Vec::new();
        if !self.salt.is_empty() {
            salt = self.salt.clone();
            salt.resize(self.salt.len().next_multiple_of(self.hash.input_block()), 0);
        }
        if block.len() == bs {
            self.hash.hash(&[&salt, block])
        } else {
            let mut padded = block.to_vec();
            padded.resize(bs, 0);
            self.hash.hash(&[&salt, &padded])
        }
    }

    /// Check `data`, the file's bytes from block `first` on, against the
    /// tree and the root hash. `data` is whole blocks except where it ends
    /// at the end of the file. `tree(offset, len)` returns `len` bytes of
    /// the stored tree from `offset`, and is asked only for the blocks
    /// these data blocks hang from.
    ///
    /// # Errors
    ///
    /// [`Error::ChecksumMismatch`] naming the file offset of the first
    /// block, or the tree offset of the first tree block, whose digest is
    /// not the one above it; and whatever `tree` returns.
    pub fn verify(
        &self,
        first: u64,
        data: &[u8],
        tree: &mut dyn FnMut(u64, usize) -> Result<Vec<u8>>,
    ) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let bs = self.block_size();
        let per = self.hashes_per_block();
        let dlen = self.hash.digest_len();
        let mut index = first;
        let mut digests: Vec<Vec<u8>> = data
            .chunks(bs as usize)
            .map(|b| self.hash_block(b))
            .collect();
        // Where a mismatch at this level is reported: the file offset of
        // a data block, then the tree offset of a tree block.
        let mut at_level: Box<dyn Fn(u64) -> u64> = Box::new(move |i| i * bs);
        for (start, count) in self.levels() {
            let last = index + digests.len() as u64 - 1;
            let (lo, hi) = (index / per, last / per);
            if hi >= count {
                return Err(Error::BadSuperblock(format!(
                    "fs-verity: block {last} is past the {count} blocks of its tree level"
                )));
            }
            let bytes = tree((start + lo) * bs, ((hi - lo + 1) * bs) as usize)?;
            for (k, digest) in digests.iter().enumerate() {
                let entry = ((index + k as u64 - lo * per) as usize) * dlen;
                if bytes[entry..entry + dlen] != digest[..] {
                    return Err(Error::ChecksumMismatch {
                        what: "a block of an fs-verity file",
                        offset: at_level(index + k as u64),
                    });
                }
            }
            digests = bytes
                .chunks(bs as usize)
                .map(|b| self.hash_block(b))
                .collect();
            index = lo;
            at_level = Box::new(move |i| (start + i) * bs);
        }
        if digests.len() != 1 || digests[0] != self.root_hash {
            return Err(Error::ChecksumMismatch {
                what: "a block of an fs-verity file",
                offset: at_level(index),
            });
        }
        Ok(())
    }
}

/// Lay `items` -- `(key offset, bytes)` in key order -- out as one blob of
/// `len` bytes starting at key offset `base`, refusing a gap, an overlap
/// or a shortfall.
pub(crate) fn assemble(
    items: impl IntoIterator<Item = (u64, Vec<u8>)>,
    base: u64,
    len: usize,
    what: &str,
) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(len);
    let end = base + len as u64;
    for (offset, bytes) in items {
        let item_end = offset.saturating_add(bytes.len() as u64);
        if item_end <= base || offset >= end {
            continue;
        }
        let have = base + out.len() as u64;
        if offset > have {
            return Err(Error::BadSuperblock(format!(
                "{what}: nothing holds bytes {} to {offset}",
                have - base
            )));
        }
        let skip = (have - offset) as usize;
        let take = ((item_end.min(end)) - have) as usize;
        if skip < bytes.len() {
            out.extend_from_slice(&bytes[skip..skip + take]);
        }
        if out.len() == len {
            break;
        }
    }
    if out.len() != len {
        return Err(Error::BadSuperblock(format!(
            "{what}: {} of {len} bytes are stored",
            out.len()
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_inode_refuses_mutation_even_with_nodatacow_flags() {
        for flags in [INODE_RO_VERITY, INODE_RO_VERITY | 3] {
            let result = refuse_mutation(123, flags);
            assert!(matches!(result, Err(Error::UnsupportedFeature(m))
                if m.contains("fs-verity") && m.contains("123")));
        }
    }

    #[test]
    fn an_unsealed_inode_can_be_changed() {
        for flags in [0, 3, 1 << 31] {
            refuse_mutation(123, flags).unwrap();
        }
    }

    /// A Merkle tree built the way the format describes it, for a
    /// descriptor whose root hash it then fills in. The kernel-made
    /// fixtures (`tests/verity_oracle.rs`) are what show this agrees with
    /// Linux; this is the arithmetic checked against itself.
    fn sealed(data: &[u8], hash: VerityHash, log: u8, salt: &[u8]) -> (Verity, Vec<u8>) {
        let mut d = vec![0u8; DESCRIPTOR_SIZE];
        d[desc::VERSION] = 1;
        d[desc::HASH_ALGORITHM] = match hash {
            VerityHash::Sha256 => 1,
            VerityHash::Sha512 => 2,
        };
        d[desc::LOG_BLOCKSIZE] = log;
        d[desc::SALT_SIZE] = salt.len() as u8;
        d[desc::DATA_SIZE..desc::DATA_SIZE + 8].copy_from_slice(&(data.len() as u64).to_le_bytes());
        d[desc::SALT..desc::SALT + salt.len()].copy_from_slice(salt);
        let mut v = Verity::parse(d, data.len() as u64).unwrap();
        let bs = v.block_size() as usize;
        let mut levels: Vec<Vec<u8>> = Vec::new();
        let mut blocks: Vec<Vec<u8>> = data.chunks(bs).map(<[u8]>::to_vec).collect();
        let root = if data.is_empty() {
            vec![0u8; hash.digest_len()]
        } else {
            loop {
                let digests: Vec<u8> = blocks.iter().flat_map(|b| v.hash_block(b)).collect();
                if blocks.len() == 1 {
                    break digests;
                }
                let mut level = digests;
                level.resize(level.len().next_multiple_of(bs), 0);
                blocks = level.chunks(bs).map(<[u8]>::to_vec).collect();
                levels.push(level);
            }
        };
        v.descriptor[desc::ROOT_HASH..desc::ROOT_HASH + root.len()].copy_from_slice(&root);
        v.root_hash = root;
        let tree: Vec<u8> = levels.into_iter().rev().flatten().collect();
        assert_eq!(tree.len() as u64, v.tree_size());
        (v, tree)
    }

    fn data(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + i / 4096) as u8).collect()
    }

    fn fetch(tree: &[u8]) -> impl FnMut(u64, usize) -> Result<Vec<u8>> + '_ {
        move |off, len| Ok(tree[off as usize..off as usize + len].to_vec())
    }

    #[test]
    fn the_levels_are_stored_root_first() {
        // 300 blocks of SHA-256: level 0 is 3 blocks, level 1 is one.
        let (v, _) = sealed(&data(300 * 4096), VerityHash::Sha256, 12, &[]);
        assert_eq!(v.levels(), vec![(1, 3), (0, 1)]);
        let (one, tree) = sealed(&data(10), VerityHash::Sha256, 12, &[]);
        assert!(one.levels().is_empty() && tree.is_empty());
    }

    #[test]
    fn every_shape_verifies_whole_and_in_part() {
        for (len, hash, salt) in [
            (0, VerityHash::Sha256, &b""[..]),
            (1, VerityHash::Sha256, &b""[..]),
            (4096, VerityHash::Sha256, &b""[..]),
            (4097, VerityHash::Sha256, &b"salt"[..]),
            (600 * 1024 + 5, VerityHash::Sha256, &b""[..]),
            (300 * 4096 + 1, VerityHash::Sha512, &[0xab; 32][..]),
            (5000 * 4096, VerityHash::Sha512, &b"x"[..]),
        ] {
            let bytes = data(len);
            let (v, tree) = sealed(&bytes, hash, 12, salt);
            v.verify(0, &bytes, &mut fetch(&tree))
                .unwrap_or_else(|e| panic!("{len} bytes whole: {e}"));
            if len > 3 * 4096 {
                let part = &bytes[4096..3 * 4096];
                v.verify(1, part, &mut fetch(&tree))
                    .unwrap_or_else(|e| panic!("{len} bytes, blocks 1 and 2: {e}"));
            }
        }
    }

    #[test]
    fn a_changed_byte_anywhere_is_refused() {
        let bytes = data(300 * 4096 + 17);
        let (v, tree) = sealed(&bytes, VerityHash::Sha256, 12, b"pepper");
        let mut bad = bytes.clone();
        bad[150 * 4096 + 3] ^= 1;
        let e = v.verify(0, &bad, &mut fetch(&tree)).unwrap_err();
        assert_eq!(
            e,
            Error::ChecksumMismatch {
                what: "a block of an fs-verity file",
                offset: 150 * 4096
            }
        );
        // A block a read does not cover is not looked at.
        v.verify(2, &bad[2 * 4096..4 * 4096], &mut fetch(&tree))
            .unwrap();
        // A tree byte: the data still matches its entry, so the level-0
        // block it sits in no longer hashes to the root level's entry.
        let mut t = tree.clone();
        t[4096 + 5] ^= 1;
        assert!(v.verify(0, &bytes, &mut fetch(&t)).is_err());
        // A different root.
        let mut r = v.clone();
        r.root_hash[0] ^= 1;
        assert!(r.verify(0, &bytes, &mut fetch(&tree)).is_err());
    }

    #[test]
    fn the_digest_ignores_the_signature_size() {
        let (v, _) = sealed(&data(10), VerityHash::Sha256, 12, &[]);
        let mut signed = v.descriptor.clone();
        signed[desc::SIG_SIZE] = 3;
        signed.extend_from_slice(b"sig");
        let s = Verity::parse(signed, 10).unwrap();
        assert_eq!(s.signature, b"sig");
        assert_eq!(s.digest(), v.digest());
        assert!(v.measurement().starts_with("sha256:"));
        assert_eq!(v.measurement().len(), "sha256:".len() + 64);
    }

    #[test]
    fn a_descriptor_that_contradicts_itself_is_refused() {
        let (v, _) = sealed(&data(10), VerityHash::Sha256, 12, &[]);
        assert!(
            Verity::parse(v.descriptor.clone(), 11).is_err(),
            "wrong size"
        );
        let mut d = v.descriptor.clone();
        d[desc::RESERVED + 7] = 1;
        assert!(Verity::parse(d, 10).is_err(), "reserved set");
        let mut d = v.descriptor.clone();
        d[desc::HASH_ALGORITHM] = 9;
        assert!(matches!(
            Verity::parse(d, 10),
            Err(Error::UnsupportedFeature(_))
        ));
        let mut d = v.descriptor.clone();
        d[desc::SIG_SIZE] = 1;
        assert!(
            Verity::parse(d, 10).is_err(),
            "a signature that is not there"
        );
        assert!(Verity::parse(vec![1; 100], 10).is_err(), "short");
    }

    #[test]
    fn items_are_laid_out_by_key_offset() {
        let items = vec![(1, vec![1, 2, 3]), (4, vec![4, 5]), (6, vec![6])];
        assert_eq!(
            assemble(items.clone(), 1, 6, "t").unwrap(),
            [1, 2, 3, 4, 5, 6]
        );
        assert_eq!(assemble(items.clone(), 2, 3, "t").unwrap(), [2, 3, 4]);
        assert!(assemble(items.clone(), 1, 7, "t").is_err(), "short");
        let gap = vec![(0, vec![1]), (2, vec![3])];
        assert!(assemble(gap, 0, 3, "t").is_err(), "gap");
    }
}
