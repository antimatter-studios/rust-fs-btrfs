# Human-code findings — status

Tracks every **High** and **Medium** finding from
[`human-code-report-2026-08-28.md`](human-code-report-2026-08-28.md). The report
predates the work; this is the current position. Updated 2026-08-30.

**27 findings** — 6 High, 10 Medium, 5 Low, plus 7 tests-that-cannot-fail. This
covers the 16 High and Medium.

| | High | Medium |
|---|---|---|
| Fixed | 3 | 2 |
| Left for a human decision | 0 | 4 |
| Fixable, not yet done | 3 | 4 |

---

## High

### H1 — `render_plan`'s doc block stated the opposite of what it does — **fixed earlier**

[#46](https://github.com/antimatter-studios/rust-fs-btrfs/pull/46).

### H2 — `fs.rs` said decompression is not implemented, in the module that decompresses — **fixed**

The module's own list of "things we refuse rather than guess at" opened with
*"Compressed extents. Decompression is not implemented."* All three btrfs codecs
— zlib, LZO and zstd — are decoded, in `compression.rs`, and `fs.rs` calls it at
two sites.

True when written; not for some time. The list no longer claims it, and the
module says so explicitly so the next reader does not have to check.

### H6 — a short `FREE_SPACE_INFO` item silently kept a stale extent count — **fixed**

```rust
let mut info = item.clone();
if info.data.len() >= 4 {
    info.data[0..4].copy_from_slice(&(runs.len() as u32).to_le_bytes());
}
```

The `if` wrote the count when the item was long enough and **said nothing when
it was not** — so a short item kept a stale count while its runs were rewritten
underneath it, and the free-space tree disagreed with itself with no signal that
anything had happened. Nothing could test the branch, because it produced no
observable effect.

Refused now. The offsets also come from `block_group::free_space_info`, which is
where the *reader* already gets them; this side was spelling them out by hand,
so the two could drift.

### H3, H4, H5 — hand-rolled leaf parsing, a duplicated free-run merge, contradictory refusal messages — **fixable, not yet done**

All three concern the write path duplicating what `btree.rs` and
`merge_adjacent` already do. H5 is the one to do first: `insert`'s refusal
message contradicts the `split` function fifty lines below it, so one of the two
is wrong about when a split happens.

---

## Medium

### M7 — six declarations of `ROOT_ITEM_KEY`, four of `root_item::BYTENR` — **fixed earlier**

[#46](https://github.com/antimatter-studios/rust-fs-btrfs/pull/46). One
declaration now.

### M10 — `leaf_edit::insert_or_split` had no callers and no tests — **fixed earlier**

### M14 — `lib.rs`'s status section described an earlier crate — **fixed earlier**

### M8, M9 — `apply_free_space` at 107 lines and four jobs; `render_plan` at 106 across four abstraction levels — **needs your decision**

Both accurate. Both are transaction-path functions where the ordering *is* the
correctness argument, and splitting them without a test on the seams trades a
readability gain for a risk in the part of the crate that writes to disk.

### M12 — `mine` reads as a set-membership test and is a full B-tree traversal — **needs your decision**

A rename is the fix and the right name depends on what the caller should
understand it to cost.

### M11, M13 — four implementations of "find a tree's root"; the read-closure boilerplate eleven times — **fixable, not yet done**

Both genuine, both mechanical, both wanting the oracle suites as the contract.

### M15 — the example invalidates the free-space tree for a reason that no longer holds — **fixable, not yet done**

The reason was that the transaction did not maintain the tree. It does now, so
the example is teaching a habit that is no longer necessary.

### M16 — `le32`/`le64`/`items_of` copied across eight test binaries that share a helper module — **fixable, not yet done**

They already share `tests/common`; the copies predate it.

---

## Verification

173 unit tests pass, unchanged in number. `chore lint` clean. H6 is the only
behavioural change: a `FREE_SPACE_INFO` item too short to hold its own count is
now refused rather than silently kept.
