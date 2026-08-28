# What a transaction actually does

The write path had five pieces — leaves, nodes, allocation, the record of an
allocation, and the superblock — and a sequencer that puts them on the device in the
order the kernel does. What it did not have was the part that decides *which* blocks a
change produces, because that part is recursive: recording an allocation modifies the
extent tree, and the extent tree lives in blocks that must themselves be allocated.

Reasoning about how the kernel breaks that cycle is how a writer ends up implementing
something plausible and wrong. So it was measured.

`scripts/build-cow-fixtures.sh` captures three whole images of one filesystem:

| image | what happened |
|---|---|
| `btrfs-cow-before.img` | `mkfs`, then one mount/unmount so the first-mount feature write is already done |
| `btrfs-cow-control.img` | mounted and unmounted again, **changing nothing** |
| `btrfs-cow-after.img` | one `touch` and one `sync` |

**A mount cycle that changes nothing does not always commit.** It did in the Debian
oracle VM, where the numbers below were measured; the same script on the CI runner left
the generation unmoved. Which is fair — there was nothing to write. So the control pair
is a measurement when it commits and an empty pair when it does not, and
`tests/transaction_oracle.rs` treats a pair with no transaction as nothing to check
rather than as a failure, while requiring that *something* committed.

The control is what makes the rest mean anything. Mounting read-write commits by
itself, so a before/after pair around a `touch` contains the touch *and* whatever a bare
mount cycle does. Without a pair that did nothing, all of it would be attributed to
creating the file.

`cargo run --example cow_diff -- <before> <after>` produces what follows.

## An empty commit is not empty

Changing nothing still rewrites four blocks:

```text
generation 8 -> 9
bytes_used 147456 -> 147456

tree blocks written
  dev              1
  extent           1
  free-space-tree  1
  root             1

extent tree items ADDED    4
extent tree items REMOVED  4
```

That is the fixed point, visible. Nothing the user did caused it:

- the **root tree** must be rewritten, because it names the other trees' roots and
  those addresses changed;
- the **extent tree** must be rewritten, to record the new blocks and release the old;
- the **free-space tree** must be rewritten, because what is free changed;
- the **dev tree** must be rewritten, because device usage changed.

And each of those four rewrites is itself an allocation, recorded in the same
transaction — which is what makes the count come out equal rather than growing without
bound. **Four in, four out.** The recursion terminates because a copy-on-write rewrite
is an allocation *and* a free, and the extent tree ends up recording its own new blocks.

## One `touch` adds two blocks

The same measurement around a real change, looking at the blocks written by the
transaction that created the file:

```text
tree blocks written
  dev              1
  extent           1
  free-space-tree  1
  root             1
  fs               2      <- the file
```

Six blocks: the four an empty commit costs, plus two in the fs tree. So the cost of a
change is the fixed floor plus what the change itself touches.

## The invariant that generalises

`bytes_used` did not move in either case, because four blocks were allocated and four
released. Stated so it holds when a tree does grow:

```text
bytes_used(after) - bytes_used(before)
    == (metadata items added - metadata items removed) * nodesize
```

Together with two others this is enough to check a transaction without knowing what it
was for:

1. every block **reachable** from the superblock — through the root tree, through every
   `ROOT_ITEM` it holds, down to the leaves — has a `METADATA_ITEM` naming it;
2. every recorded block is physically on the disk and checksums;
3. `bytes_used` equals the sum of every block group's `used`, and moves by the delta
   above.

Reachability rather than generation, and that distinction cost two CI runs. "Every block
stamped with the current generation must be recorded" is the obvious cheap test and it is
wrong: a block written by one transaction in a sequence is routinely superseded by the
next, and is then still on the disk, still checksumming, and correctly unrecorded.
Generation says when a block was written, not whether anything still points at it.

`tests/transaction_oracle.rs` asserts all three against the kernel's own transactions.
It is written now, before the transaction planner exists, precisely so the planner has
something to be measured against that was not written to fit it.

## What is still not measured

A commit that makes a tree grow taller, and a data write — which adds a checksum-tree
leaf and data extents, neither of which appears above. The document's earlier reasoning
puts those before the tree blocks in write order; that part is still reasoned.
