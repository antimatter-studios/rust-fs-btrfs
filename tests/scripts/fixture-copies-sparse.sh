#!/usr/bin/env bash
#
# fixture-copies-sparse.sh -- a test copies a fixture with copy_sparse, unless
# this list says its plain copy was measured and fits (#747).
#
# std::fs::copy WRITES A SPARSE FIXTURE OUT IN FULL ON LINUX: an image holding
# 36 MiB takes all of its 512 MiB in the copy. About one x86_64 job in fifty
# lands on a runner with 14 GB free, where the kernel tier starts with 6877 MB,
# and the job then fails with "No space left on device" in whichever test copies
# next (merge queue run 41715770227, tests/pool_write_kernel.rs). A runner with
# 86 GB passes either way, so only a rule read from the sources can hold this.
#
# THE LIST IS WHAT STAYED PLAIN, each with the most MB its target used at once
# when every target was measured in the guest. Each one is under 600 MB and
# gives its copy back. A new copy goes through copy_sparse, or joins the list
# with its own measurement. A count here has to match the sources exactly, so a
# copy moved to copy_sparse leaves the list in the same change.
#
#   bash tests/scripts/fixture-copies-sparse.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0
ok()   { echo "ok $*"; }
fail() { echo "not ok $*" >&2; fails=$(( fails + 1 )); }

# file under tests/, plain copies in it, MB measured.
MEASURED="
block_group_tree_write_kernel.rs 1 441
capi_defrag.rs 1 141
capi_subvol_create_kernel.rs 1 513
capi_subvol_snapshot_kernel.rs 1 513
cli_defrag.rs 1 264
cli_xattr_kernel.rs 1 512
crash_consistency.rs 1 258
kernel_readback.rs 1 512
shared_tree_block_release.rs 1 522
snapshot_shared_write.rs 1 512
subvol_create_kernel.rs 1 513
subvol_delete_kernel.rs 1 513
subvol_readonly_kernel.rs 1 513
trim_oracle.rs 1 368
verity_oracle.rs 1 418
verity_write_kernel.rs 1 531
"

# Every line of $1's .rs files that calls a plain copy, as `file count`, comments
# left out: a comment explaining the rule is not a copy.
plain_copies() {
    (
        cd "$1" || exit 1
        find . -name '*.rs' | LC_ALL=C sort | while IFS= read -r file; do
            count="$(grep -E '\bfs::copy\b' "$file" | grep -cvE '^[[:space:]]*//')"
            [ "$count" -eq 0 ] || printf '%s %s\n' "${file#./}" "$count"
        done
    )
}

# Say what differs between the plain copies under $1 and the list. Prints a line
# per difference, and nothing when they agree.
check() {
    found="$(plain_copies "$1")"
    echo "$found" | while read -r file count; do
        [ -n "$file" ] || continue
        listed="$(echo "$MEASURED" | awk -v f="$file" '$1 == f { print $2 }')"
        if [ -z "$listed" ]; then
            echo "tests/$file: $count plain fs::copy, use copy_sparse or measure it"
        elif [ "$listed" != "$count" ]; then
            echo "tests/$file: $count plain fs::copy, the list says $listed"
        fi
    done
    echo "$MEASURED" | while read -r file count _; do
        [ -n "$file" ] || continue
        echo "$found" | awk -v f="$file" '$1 == f { hit = 1 } END { exit !hit }' ||
            echo "tests/$file: listed with $count plain fs::copy, the sources have none"
    done
}

differences="$(check "$REPO/tests")"
if [ -z "$differences" ]; then
    ok "every plain fs::copy under tests/ is on the measured list"
else
    fail "plain fs::copy calls the list does not account for:"
    echo "$differences" >&2
fi

# A gate that cannot fail is no gate: the same check over trees made to break it.
mkdir -p "$REPO/tmp"
sandbox="$(mktemp -d "$REPO/tmp/fixture-copies-sparse.XXXXXX")"
trap 'rm -rf "$sandbox"' EXIT

listed_tree() {
    rm -rf "$sandbox/tests"
    mkdir "$sandbox/tests"
    echo "$MEASURED" | while read -r file count _; do
        [ -n "$file" ] || continue
        i=0
        while [ "$i" -lt "$count" ]; do
            echo 'std::fs::copy(from, to).unwrap();' >>"$sandbox/tests/$file"
            i=$(( i + 1 ))
        done
    done
}

listed_tree
if [ -z "$(check "$sandbox/tests")" ]; then
    ok "a tree holding exactly the list passes"
else
    fail "a tree holding exactly the list was refused"
fi

listed_tree
echo 'std::fs::copy(fixture("btrfs-pool-a.img"), &image).unwrap();' >"$sandbox/tests/new_kernel.rs"
case "$(check "$sandbox/tests")" in
    *new_kernel.rs*) ok "a plain copy in a file off the list is refused" ;;
    *) fail "a plain copy in a file off the list passed" ;;
esac

listed_tree
mkdir "$sandbox/tests/nested"
printf 'use std::fs;\nfs::copy(a, b).unwrap();\n' >"$sandbox/tests/nested/mod.rs"
case "$(check "$sandbox/tests")" in
    *nested/mod.rs*) ok "a plain copy in a subdirectory, called through fs::, is refused" ;;
    *) fail "a plain copy in a subdirectory passed" ;;
esac

listed_tree
echo 'std::fs::copy(from, to).unwrap();' >>"$sandbox/tests/trim_oracle.rs"
case "$(check "$sandbox/tests")" in
    *trim_oracle.rs*) ok "one more copy in a listed file is refused" ;;
    *) fail "one more copy in a listed file passed" ;;
esac

listed_tree
rm "$sandbox/tests/trim_oracle.rs"
case "$(check "$sandbox/tests")" in
    *trim_oracle.rs*) ok "a listed copy the sources no longer hold is refused" ;;
    *) fail "a listed copy the sources no longer hold passed" ;;
esac

listed_tree
printf 'copy_sparse(from, to).unwrap();\n// std::fs::copy(a, b) writes it out in full.\n' >"$sandbox/tests/new_kernel.rs"
if [ -z "$(check "$sandbox/tests")" ]; then
    ok "copy_sparse and a comment naming fs::copy both pass"
else
    fail "copy_sparse or a comment naming fs::copy was refused"
fi

[ "$fails" -eq 0 ] || { echo "fixture-copies-sparse.sh: $fails failed" >&2; exit 1; }
echo "fixture-copies-sparse.sh: all checks passed"
