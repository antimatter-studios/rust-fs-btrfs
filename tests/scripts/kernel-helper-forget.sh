#!/usr/bin/env bash
#
# kernel-helper-forget.sh — every kernel oracle script that attaches a copied
# image waits for udev and then makes the kernel forget earlier scans first
# (#738, #742, #754): `guest_script`, each variant of
# `guest_kernel_read_variants`, `pool_run`, and tests/support/check-pool.sh.
#
# THE ORDER IS THE FIX, as it is in `mount_copy` in
# test-disks/guest-build-images.sh (#736). A udev worker still scanning when
# the last filesystem was unmounted records that filesystem's newer
# generation, so a forget that runs ahead of the worker is undone by it.
#
# tests/kernel_oracle_forgets_scans.rs makes the real kernel refuse a copy and
# shows the forget lifts the refusal. It runs only in the VM, and it cannot
# force a scan to race the forget. So this guard, which needs nothing but the
# source, holds the order.
#
#   bash tests/scripts/kernel-helper-forget.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HELPER="$REPO/tests/support/src/kernel.rs"
CHECK_POOL="$REPO/tests/support/check-pool.sh"
fails=0
ok()   { echo "ok $*"; }
fail() { echo "not ok $*" >&2; fails=$(( fails + 1 )); }

# order_of <file> [function]: "good", or what is wrong with the order in that
# function, or with no function in the whole file, which is then one script.
#
# Only a line that is exactly `}` ends the function: a shell function inside a
# script's raw string closes with `}}` in the first column, and in two of these
# functions that comes ahead of the attach.
#
# A sweep attaches once per variant, so what runs ahead of its `variant()`
# happens only once and does not count.
order_of() {
    awk -v opening="${2:+fn $2(}" '
        opening == "" { inside = 1 }
        opening != "" && /^(pub )?fn / && index($0, opening) { inside = 1 }
        opening != "" && inside && /^}$/ { inside = 0 }
        !inside { next }
        /^variant\(\) \{\{$/ { settle = forget = attach = 0 }
        !settle && /^[ \t]*udevadm settle$/ { settle = FNR }
        !forget && /^[ \t]*btrfs device scan --forget >\/dev\/null$/ { forget = FNR }
        !attach && /^[ \t]*(loop=|loops\+=\()"\$\(losetup --find/ { attach = FNR }
        END {
            if (!attach) print "no losetup --find"
            else if (!settle) print "no udevadm settle"
            else if (!forget) print "no checked btrfs device scan --forget"
            else if (settle > forget) print "the forget runs ahead of the settle"
            else if (forget > attach) print "the loop device is attached ahead of the forget"
            else print "good"
        }
    ' "$1"
}

for function in guest_script guest_kernel_read_variants pool_run; do
    verdict="$(order_of "$HELPER" "$function")"
    [ "$verdict" = good ] \
        && ok "$function settles, forgets, then attaches" \
        || fail "$function settles, forgets, then attaches: $verdict"
done

verdict="$(order_of "$CHECK_POOL")"
[ "$verdict" = good ] \
    && ok "check-pool.sh settles, forgets, then attaches" \
    || fail "check-pool.sh settles, forgets, then attaches: $verdict"

# --- the guard can fail --------------------------------------------------
sandbox="$(mktemp -d)"
trap 'rm -rf "$sandbox"' EXIT
planted="$sandbox/kernel.rs"

printf 'fn guest_script() {\nudevadm settle\nbtrfs device scan --forget >/dev/null\nloop="$(losetup --find)"\n}\n' > "$planted"
[ "$(order_of "$planted" guest_script)" = good ] \
    && ok "the right order is accepted" \
    || fail "the right order is accepted: $(order_of "$planted" guest_script)"

printf 'fn guest_script() {\nloop="$(losetup --find)"\n}\n' > "$planted"
[ "$(order_of "$planted" guest_script)" = "no udevadm settle" ] \
    && ok "a helper with neither is refused" \
    || fail "a helper with neither is refused: $(order_of "$planted" guest_script)"

printf 'fn guest_script() {\nudevadm settle\nbtrfs device scan --forget >/dev/null 2>&1 || true\nloop="$(losetup --find)"\n}\n' > "$planted"
[ "$(order_of "$planted" guest_script)" = "no checked btrfs device scan --forget" ] \
    && ok "a forget whose failure is ignored is refused" \
    || fail "a forget whose failure is ignored is refused: $(order_of "$planted" guest_script)"

printf 'fn guest_script() {\nbtrfs device scan --forget >/dev/null\nudevadm settle\nloop="$(losetup --find)"\n}\n' > "$planted"
[ "$(order_of "$planted" guest_script)" = "the forget runs ahead of the settle" ] \
    && ok "a forget ahead of the settle is refused" \
    || fail "a forget ahead of the settle is refused: $(order_of "$planted" guest_script)"

printf 'fn guest_script() {\nudevadm settle\nloop="$(losetup --find)"\nbtrfs device scan --forget >/dev/null\n}\n' > "$planted"
[ "$(order_of "$planted" guest_script)" = "the loop device is attached ahead of the forget" ] \
    && ok "an attach ahead of the forget is refused" \
    || fail "an attach ahead of the forget is refused: $(order_of "$planted" guest_script)"

printf 'fn other() {\nudevadm settle\nbtrfs device scan --forget >/dev/null\n}\nfn guest_script() {\nloop="$(losetup --find)"\n}\n' > "$planted"
[ "$(order_of "$planted" guest_script)" = "no udevadm settle" ] \
    && ok "a forget in another function does not count" \
    || fail "a forget in another function does not count: $(order_of "$planted" guest_script)"

printf 'pub fn sweep(\n    base: &str,\n) -> Vec<u8> {\ncleanup() {{\n}}\nvariant() {{\n    udevadm settle\n    btrfs device scan --forget >/dev/null\n    loop="$(losetup --find --show)"\n}}\n}\n' > "$planted"
[ "$(order_of "$planted" sweep)" = good ] \
    && ok "a public function, indented, past a shell function's closing brace, is accepted" \
    || fail "a public function, indented, past a shell function's closing brace, is accepted: $(order_of "$planted" sweep)"

printf 'fn sweep() {\nudevadm settle\nbtrfs device scan --forget >/dev/null\nvariant() {{\n    loop="$(losetup --find --show)"\n}}\n}\n' > "$planted"
[ "$(order_of "$planted" sweep)" = "no udevadm settle" ] \
    && ok "a settle and forget ahead of variant() is refused" \
    || fail "a settle and forget ahead of variant() is refused: $(order_of "$planted" sweep)"

printf 'fn pool() {\nudevadm settle\nbtrfs device scan --forget >/dev/null\nfor img in a b; do\n    loops+=("$(losetup --find --show)")\ndone\n}\n' > "$planted"
[ "$(order_of "$planted" pool)" = good ] \
    && ok "a pool attaching its members in a loop is accepted" \
    || fail "a pool attaching its members in a loop is accepted: $(order_of "$planted" pool)"

printf 'fn pool() {\nudevadm settle\nfor img in a b; do\n    loops+=("$(losetup --find --show)")\ndone\nbtrfs device scan --forget >/dev/null\n}\n' > "$planted"
[ "$(order_of "$planted" pool)" = "the loop device is attached ahead of the forget" ] \
    && ok "a pool attaching its members ahead of the forget is refused" \
    || fail "a pool attaching its members ahead of the forget is refused: $(order_of "$planted" pool)"

printf 'fn pool_other() {\nudevadm settle\nbtrfs device scan --forget >/dev/null\nloop="$(losetup --find)"\n}\n' > "$planted"
[ "$(order_of "$planted" pool)" = "no losetup --find" ] \
    && ok "a function whose name only starts the same does not count" \
    || fail "a function whose name only starts the same does not count: $(order_of "$planted" pool)"

printf '#!/usr/bin/env bash\ncleanup() {\n    true\n}\ntrap cleanup EXIT\nudevadm settle\nbtrfs device scan --forget >/dev/null\nfor image in "$@"; do\n    loops+=("$(losetup --find --show --read-only "$image")")\ndone\n' > "$planted"
[ "$(order_of "$planted")" = good ] \
    && ok "a script past its own function's closing brace is accepted" \
    || fail "a script past its own function's closing brace is accepted: $(order_of "$planted")"

printf '#!/usr/bin/env bash\nfor image in "$@"; do\n    loops+=("$(losetup --find --show --read-only "$image")")\ndone\n' > "$planted"
[ "$(order_of "$planted")" = "no udevadm settle" ] \
    && ok "a script with neither is refused" \
    || fail "a script with neither is refused: $(order_of "$planted")"

printf '#!/usr/bin/env bash\nudevadm settle\nbtrfs device scan --forget >/dev/null 2>&1 || true\nfor image in "$@"; do\n    loops+=("$(losetup --find --show --read-only "$image")")\ndone\n' > "$planted"
[ "$(order_of "$planted")" = "no checked btrfs device scan --forget" ] \
    && ok "a script whose forget's failure is ignored is refused" \
    || fail "a script whose forget's failure is ignored is refused: $(order_of "$planted")"

printf '#!/usr/bin/env bash\nudevadm settle\nfor image in "$@"; do\n    loops+=("$(losetup --find --show --read-only "$image")")\ndone\nbtrfs device scan --forget >/dev/null\n' > "$planted"
[ "$(order_of "$planted")" = "the loop device is attached ahead of the forget" ] \
    && ok "a script attaching ahead of its forget is refused" \
    || fail "a script attaching ahead of its forget is refused: $(order_of "$planted")"

if [ "$fails" -gt 0 ]; then
    echo "FAIL  $fails kernel-helper-forget violation(s)" >&2
    exit 1
fi
echo "PASS  every kernel oracle script settles and forgets before it attaches a copy"
