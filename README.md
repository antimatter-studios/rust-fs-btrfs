# rust-fs-btrfs

Pure-Rust, clean-room [Btrfs](https://btrfs.readthedocs.io/) driver. A reader for
the Btrfs on-disk format built over the shared
[`am-fs-core`](https://github.com/antimatter-studios/rust-fs-core) block-device
trait, exposing a stable C ABI (`fs_btrfs_*`) for FFI from C/C++, Swift or Go.

Published on crates.io as `am-fs-btrfs`; the library name is `fs_btrfs`.

Btrfs is a copy-on-write filesystem: nothing is overwritten in place, every
structure is a B-tree, and the physical location of any byte is resolved through
the chunk tree rather than computed from a fixed formula. That makes it a
different shape of problem from ext4 or NTFS — a reader has to bootstrap the
chunk tree out of the superblock's embedded system chunk array before it can
address anything else at all.

- **Clean-room** — written from the published on-disk format, not translated
  from kernel or `btrfs-progs` source
- **Permissive** — MIT, with a permissive dependency tree (no GPL/LGPL anywhere)
- **Cross-platform** — the driver reads images on Linux, macOS and Windows; only
  the test oracle needs a Linux kernel

## Status

Under active development. The table is the honest state of what is implemented,
not a roadmap.

It was last wrong in the other direction: it described a reader with the write
path "out of scope" and B-tree traversal, inodes and compression "planned", all
of which had been in for some time. A status table that understates is not
harmless — someone deciding whether this is usable reads it and concludes it is
not.

| Area | Support |
|------|---------|
| Superblock (primary at 64 KiB) | done — 216 field comparisons against dump-super |
| Superblock mirrors (64 MiB, 256 GiB) | parsed; mirror-selection policy pending |
| Checksum: **crc32c** | done |
| Checksum: **xxhash64**, **sha256**, **blake2b** | done — all four verified against real media |
| System chunk array → chunk tree bootstrap | done |
| Chunk tree / logical→physical mapping | done — bootstrap array, then the full tree folded in |
| B-tree node + leaf traversal | done — walk and keyed search, on multi-level trees |
| Root tree, fs tree, extent tree | done |
| Inodes, directory items, extent data | done |
| Directory listing, lookup, path resolution | done |
| Symlinks | done |
| Extended attributes: read | done — `list_xattrs` / `get_xattr`, checked name by name against `getfattr` |
| Extended attributes: write | not yet — setting one means inserting into a tree a transaction has to commit |
| Profiles: single, dup, raid0, raid1, raid10 | done |
| Profiles: raid5/6 | refused explicitly, not guessed |
| Mixed block groups (`mkfs.btrfs -M`) | reads; covered by the fixture matrix |
| Subvolumes and snapshots: listing | done — id, path, parent, snapshot and read-only flags, checked against `btrfs subvolume list` |
| Subvolumes and snapshots: reading inside one | done — `open_subvolume` gives a handle over that tree; read-only |
| A path that crosses into a subvolume | not yet — `lookup_path("/sub/x")` stops at the boundary and says which subvolume to open. An inode number means nothing without its tree, so crossing has to hand back both |
| Compression (zlib / lzo / zstd extents) | done — all three, verified against files the kernel wrote |
| Write path: overwrite in place | done — `nodatacow` files only, no journal needed |
| Write path: anything copy-on-write | planned — see `docs/transaction-format.md` |
| C ABI (`fs_btrfs_*`) | done, including the write entry points |

## Test contract

Two layers, and only one of them can tell you the driver is right.

**Layer 1 — unit tests.** Fast, hermetic, no external tooling. They run on every
`cargo test` and prove the parser is *self-consistent*: that it accepts the
fixtures the crate builds for itself and reports the values those fixtures
encode.

That is a weaker claim than it looks. When a fixture is hand-built from the same
reading of the spec as the parser, a misread field is encoded wrong and decoded
wrong in exactly the same way, and the assertion passes. Byte-order slips,
transposed magic values, checksums computed over the wrong span — none of them
disturb a round-trip. A green unit suite means the driver is consistent with
itself, not that it reads Btrfs.

**Layer 2 — the real-kernel gate.** Real filesystems, built by the canonical
`mkfs.btrfs`, described by `btrfs inspect-internal dump-super`, checked by
`btrfs check`, and mounted by the in-kernel Btrfs driver. The driver then parses
those same images and must agree with the reference dump field by field.

This layer is **blocking in CI** (`.github/workflows/ci.yml`, gated by the
always-run `ci-ok` check), not an optional confirmation. The gate builds the full
fixture matrix, loop-mounts every image, writes and reads a file back through the
kernel, performs a whole transaction with this crate and has `btrfs check` and a
real mount judge the result, and fails if the kernel logs a single btrfs warning
— a mount that succeeds while the kernel complains is not a pass.

The case for making it blocking is empirical. In the sister
[XFS driver](https://github.com/antimatter-studios/rust-fs-xfs) the equivalent
gate found **three live parser bugs on its first run**, with the entire unit
suite green: a superblock magic with two bytes transposed, checksums stored
little-endian while every other field is big-endian, and a checksum computed over
the structure rather than the whole sector. Each is invisible to a round-trip
test and fatal against a real filesystem. There is no reason to expect Btrfs —
with more indirection, more checksum algorithms and more layout variation — to be
kinder.

**All of it happens inside the [fs-linux-test-harness][harness] VM**, and that is
the part worth knowing. Every `mkfs.btrfs`, every `btrfs check`, every `btrfs
inspect-internal` and every loop mount runs in one pinned Debian guest, on a
developer's machine exactly as in CI. It used to run on the CI runner under
`sudo`, which meant the kernel half of this gate could not be run on a Mac at
all, and on Linux only by handing a test suite root. The consequence you can see
from a laptop: `chore test` is the gate, wherever you are.

[harness]: https://github.com/antimatter-studios/fs-linux-test-harness

**Nothing skips.** A missing fixture, a missing tool or a VM that will not boot
fails the test that needed it, naming the task that provides it. That is not
tidiness either: this repository's leaf oracle once ran against no fixtures at
all and reported green, and the compression and nodatacow oracles found nothing
on every pull request for the same reason ([#69][i69]).

[i69]: https://github.com/antimatter-studios/rust-fs-btrfs/issues/69

### The geometry matrix

One list, in `test-disks/guest-build-images.sh`, built by one builder inside the
harness guest — so the gate a developer runs locally and the gate that guards the
branch cannot cover different ground. (There used to be two builders, and they
disagreed.)

| Fixture | `mkfs.btrfs` args | What it moves |
|---------|-------------------|---------------|
| `default` | — | the baseline the tooling picks for itself |
| `node4k` / `node16k` | `-n 4096` / `-n 16384` | every b-tree item offset |
| `csum-crc32c` | `--csum crc32c` | the classic checksum, 4 bytes used of 32 |
| `csum-xxhash` | `--csum xxhash` | 8-byte digest in a 32-byte field |
| `csum-sha256` | `--csum sha256` | full-width digest |
| `csum-blake2` | `--csum blake2` | full-width digest, different algorithm id |
| `single` | `-d single -m single` | explicit single profile, chunk-tree layout |
| `dup` | `-d dup -m dup` | duplicated data and metadata block groups |
| `mixed` | `-M` | data and metadata folded into one block-group type |

Each fixture is a ~400 MiB image written as `test-disks/btrfs-<name>.img` beside
its `test-disks/btrfs-<name>.superdump`. A geometry `mkfs.btrfs` refuses **fails
the build** — a fixture that quietly stopped being generated is a hole in the gate
that still reports green.

Beyond the matrix, the same builder produces the fixtures that need a *mount* to
populate: two filesystems filled until the fs tree is three levels deep, a
compressing mount per algorithm (zlib, LZO, zstd), subvolumes and snapshots,
extended attributes including a hash collision, a `chattr +C` file with no
checksums, a leaf split caught either side, a commit captured six times over, and
a two-device RAID1 pool. `test-disks/build-fixtures.sh --artefacts` lists every
file a full build produces.

## Running the gate

The same three commands on Linux and on macOS, because the Linux part happens in
the guest either way:

```sh
chore siblings   # ../rust-fs-core and ../fs-linux-test-harness, at their pinned refs
chore tools      # what the HOST needs: ripgrep, and the VM (Vagrant, QEMU, KVM/HVF)
chore fixtures   # every fixture, built by the real kernel inside the guest
chore test       # the whole gate, exactly as CI runs it
```

`chore tools` prints the exact install command for anything the host is missing.
On macOS `chore test` notices the host is not Linux and runs `chore test:vm`
instead — the same sources, compiled and run inside the guest.

### The tiers

`chore test` is the whole thing; each tier can be run on its own while working:

| Task | What it runs |
|------|--------------|
| `chore test:unit` | needs no tool, no fixture and no VM — the debug profile, which is the one that traps an arithmetic overflow |
| `chore test:images` | reads a fixture, needs no VM; this is what a machine without KVM can still run |
| `chore test:oracle` | the driver writes, btrfs-progs reads back, every tool call inside the guest |
| `chore test:kernel` | the driver writes, **the real kernel** reads back: our images loop-mounted in the guest |
| `chore test:vm` | the whole suite compiled and run inside the guest |
| `chore test:scripts` | the shell tests |

Which tier a test belongs to is derived from the test itself
(`scripts/test-targets.sh`), not from a list someone maintains: a suite is in the
gate the moment its file is committed. The previous arrangement named every suite
by hand in the workflow, and two suites that arrived after the list ran nowhere
at all ([#70][i70]).

[i70]: https://github.com/antimatter-studios/rust-fs-btrfs/issues/70

### Output

A task prints a verdict, not a transcript: a line per tier, a final count, and
the path of the log under `tmp/logs/` that holds everything else. `chore test --
--verbose` streams the whole run. Each tier carries a **measured output budget**,
and a tier that prints more than it is allowed to fails the build (exit 65) —
the table is at the top of `chores.yml`.

## Lint

```sh
chore lint       # cargo fmt --check, and clippy with -D warnings
```

## Building

The crate has a path dependency on the sibling `am-fs-core` repository. Clone it
alongside this one:

```sh
git clone https://github.com/antimatter-studios/rust-fs-core.git ../rust-fs-core
cargo build --release
```

## License

MIT — see [LICENSE](LICENSE).
