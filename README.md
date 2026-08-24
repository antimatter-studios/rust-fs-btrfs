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

Under active development. The table is the honest state of the on-disk reader,
not a roadmap.

| Area | Support |
|------|---------|
| Superblock (primary at 64 KiB) | in progress |
| Superblock mirrors (64 MiB, 256 GiB) | in progress |
| Checksum: **crc32c** | in progress |
| Checksum: **xxhash64**, **sha256**, **blake2b** | planned |
| System chunk array → chunk tree bootstrap | planned |
| Chunk tree / logical→physical mapping | planned |
| B-tree node + leaf traversal | planned |
| Root tree, fs tree, extent tree | planned |
| Inodes, directory items, extent data | planned |
| Profiles: single, dup | planned |
| Profiles: raid0/1/10/5/6 | not planned yet |
| Mixed block groups (`mkfs.btrfs -M`) | planned |
| Subvolumes and snapshots | planned |
| Compression (zlib / lzo / zstd extents) | planned |
| Write path | **out of scope** — this is a reader |

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

This layer is **blocking in CI** (`.github/workflows/ci.yml`, the
`validate against kernel btrfs driver` job), not an optional confirmation. The
job builds the full geometry matrix, loop-mounts every image, writes and reads a
file back through the kernel, and fails if the kernel logs a single btrfs
warning — a mount that succeeds while the kernel complains is not a pass.

The case for making it blocking is empirical. In the sister
[XFS driver](https://github.com/antimatter-studios/rust-fs-xfs) the equivalent
gate found **three live parser bugs on its first run**, with the entire unit
suite green: a superblock magic with two bytes transposed, checksums stored
little-endian while every other field is big-endian, and a checksum computed over
the structure rather than the whole sector. Each is invisible to a round-trip
test and fatal against a real filesystem. There is no reason to expect Btrfs —
with more indirection, more checksum algorithms and more layout variation — to be
kinder.

### The geometry matrix

One list, in `scripts/fixture-geometries.sh`, consumed by both fixture builders
so the gate a developer runs locally and the gate that guards the branch cover
the same ground:

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

Each fixture is a ~400 MiB image written as `.vm-share/btrfs-<name>.img` beside
its `.vm-share/btrfs-<name>.superdump`. A geometry the local `btrfs-progs` refuses
is **reported loudly and counted**, never silently dropped — a fixture that
quietly stopped being generated is a hole in the gate that still reports green.

## Running the gate

### On Linux

The runner is the oracle; no VM is involved.

```sh
sudo apt-get install -y btrfs-progs
bash scripts/build-fixtures-native.sh
cargo test --test oracle_vm_fixtures -- --nocapture
```

### On macOS

`mkfs.btrfs`, `btrfs check` and the in-kernel driver are Linux-only, so the
fixtures are built inside a Debian arm64 VM (QEMU with HVF — hardware
accelerated, no third-party hypervisor to install). The comparison itself still
runs on the host, so the iterate-and-check loop stays fast: the VM is only needed
when fixtures are regenerated, not on every `cargo test`.

```sh
brew install antimatter-studios/tap/qemu
brew install antimatter-studios/tap/virtiofsd
vagrant plugin install vagrant-qemu-christhomas
vagrant plugin install vagrant-notify-forwarder-christhomas

./scripts/vm.sh up               # boot the oracle (first run provisions)
./scripts/vm-build-fixtures.sh   # build the matrix into .vm-share
cargo test --test oracle_vm_fixtures -- --nocapture
```

`scripts/vm.sh` also takes `run <cmd>`, `share`, `put <file>`, `down` and
`destroy`. The VM is deliberately left running between invocations — booting is
the slow part, and an iterate-and-check loop should pay it once.

Fixtures are gitignored and absent on a fresh clone, so the oracle test skips
rather than fails when `.vm-share` is empty.

## Tests

```sh
cargo test                                          # unit tests, no external tools
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --test oracle_vm_fixtures -- --nocapture # needs fixtures (see above)
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
