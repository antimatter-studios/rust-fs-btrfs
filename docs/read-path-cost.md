# What a read costs

Measured by `tests/read_path_cost.rs`, which counts **calls to the
device** rather than wall time. Wall time on a laptop with a warm page
cache says more about the laptop than the driver; call counts are
deterministic — the same image walked the same way makes the same calls
every time — so they can be compared across months and asserted on.

Wall time is printed beside them because it is what a user feels. It is
not what anything is judged by, except where it is the only thing left
to judge by — which, as it turns out, is this driver's situation.

## 2026-09-06 — the first measurement

Two fixtures, because they say different things.

### `btrfs-rich.img` — a varied tree, 8 entries, 6 files

| shape | reads | bytes | wall |
|---|---:|---:|---:|
| mount | 4 | 53 KB | 3523 µs |
| walk | 0 | 0 | 54 µs |
| stat | 0 | 0 | 21 µs |
| read | 8 | 2.13 MB | 27605 µs |

### `btrfs-deep4k.img` — 20,000 files in `/many/`, walk bounded at 400

| shape | reads | bytes | wall |
|---|---:|---:|---:|
| mount | 2573 | 10.5 MB | 416918 µs |
| walk | 0 | 0 | 375431 µs |
| stat | 0 | 0 | 373342 µs |
| read | 0 | 0 | 371542 µs |

## What the numbers say

**Every metadata read happens at mount.** `Filesystem` loads every item
of the fs tree into a `BTreeMap` when it opens the volume, and answers
from memory afterwards. So a walk, a stat and a read of already-loaded
items make **zero** calls to the device. That is not a cache warming
up; there is nothing left to fetch.

**The mount cost scales with the volume, not with the work.** Four
reads and 53 KB for the small fixture; 2573 reads, 10.5 MB and **417
milliseconds** for one with 20,000 files, before a single question has
been asked. A volume with a million files is the same arithmetic with
two more zeros, and the memory it holds grows the same way. Opening a
filesystem in Finder to look at one directory pays for all of it.

**The remaining time is not I/O at all.** The walk of 401 items takes
375 ms and touches the device zero times. So does the stat of 400
paths, and so does the read. Three different shapes, each about 373 ms,
none of them waiting on a disk: the time is spent in the driver,
scanning the item map. That is `#64` — a lookup that scans rather than
descends — and it is now the dominant cost of every operation.

## Why there is no block cache

`am-fs-core`'s `CachingDevice` was wired in and then deliberately turned
off; `DEFAULT_CACHE_BLOCKS` is `0`. Two measured reasons:

1. **There is nothing for it to serve.** The repeat metadata reads a
   cache exists to absorb do not happen — the eager load already
   removed them.
2. **It makes the mount worse.** Metadata is read a node at a time, and
   a node is `nodesize`, typically four sectors. A cache sized in
   sectors splits one call into four: the `rich` fixture's mount asked
   the device for 4 reads uncached and **14** cached.

`Filesystem::mount_with_cache` stays, so the measurement can take both
passes and so the decision can be re-taken against a number rather than
an opinion. If the eager load is ever replaced by lazy descent, a cache
becomes worth having and that constant should change with it.

## What to fix, in order

1. **The lookup scan (#64).** Every shape now costs the same ~370 ms
   with no I/O, which means the item map is being scanned rather than
   searched. Biggest gain available and it touches no on-disk format.
2. **The eager mount.** 417 ms and 10.5 MB for 20,000 files is a
   figure that only goes one way as volumes get real. Making the tree
   lazy is a larger change and it is the one that would make a block
   cache worth having.

## How to take the measurement again

```sh
cargo test --release --test read_path_cost -- --nocapture
```

It skips without fixtures. Build them with the scripts in
`tests/scripts`.
