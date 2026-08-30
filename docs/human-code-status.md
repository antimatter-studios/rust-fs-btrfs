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

### H5 — a refusal that told callers to give up on something the crate does — **fixed**

`insert` refused an over-large item with:

> Splitting a leaf is not implemented — where the kernel puts the boundary is a
> policy this has not measured.

Both halves false. `leaf_edit::split` is **fifty lines below**, with
`tests/split_oracle.rs` checking it against leaves the kernel really split, and
`insert_or_split` does exactly what the message says cannot be done. The module
doc directly above prints three *measured* splits, and `docs/cow-transaction.md`
has a section on the measurement and on why the boundary is deliberately not
copied.

The house style is what made this expensive: the message is long and specific
precisely so a caller knows what to do next, and what it told them to do was give
up. It now names `insert_or_split` and says why `insert` itself refuses — a caller
who meant to write exactly one block should find out rather than get two.

`tree_write::build_leaf` had the milder version of the same disagreement inside one
function: its doc says splitting is "the caller's decision, not this function's"
(correct) while its error said it "is not implemented". That one now names both
`leaf_edit::split` and `insert_or_split`.

**The two tests that pinned the false sentence now assert the condition instead.**
That is the part worth keeping: `src/leaf_edit.rs:286` and
`tests/leaf_edit_oracle.rs:254` both matched on the exact wording, so the claim had
two tests holding it in place. They check that the refusal names what went wrong
and names the function that handles it — properties a correct message keeps and a
rewording cannot break.

### H3, H4 — hand-rolled leaf parsing and a duplicated free-run merge — **fixable, not yet done**

Both concern the write path duplicating what `btree.rs` and `merge_adjacent`
already do.

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
