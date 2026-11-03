#!/usr/bin/env bash
#
# kernel-helper-forget.sh — the kernel oracle helper waits for udev and then
# makes the kernel forget earlier scans, before it attaches a copied image
# (#738).
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
fails=0
ok()   { echo "ok $*"; }
fail() { echo "not ok $*" >&2; fails=$(( fails + 1 )); }

# order_of <file>: "good", or what is wrong with the order in `guest_script`.
order_of() {
    awk '
        /^fn guest_script\(/ { inside = 1 }
        inside && /^}/ { inside = 0 }
        !inside { next }
        !settle && /^udevadm settle$/ { settle = FNR }
        !forget && /^btrfs device scan --forget >\/dev\/null$/ { forget = FNR }
        !attach && /^loop="\$\(losetup --find/ { attach = FNR }
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

verdict="$(order_of "$HELPER")"
[ "$verdict" = good ] \
    && ok "guest_script settles, forgets, then attaches" \
    || fail "guest_script settles, forgets, then attaches: $verdict"

# --- the guard can fail --------------------------------------------------
sandbox="$(mktemp -d)"
trap 'rm -rf "$sandbox"' EXIT
planted="$sandbox/kernel.rs"

printf 'fn guest_script() {\nudevadm settle\nbtrfs device scan --forget >/dev/null\nloop="$(losetup --find)"\n}\n' > "$planted"
[ "$(order_of "$planted")" = good ] \
    && ok "the right order is accepted" \
    || fail "the right order is accepted: $(order_of "$planted")"

printf 'fn guest_script() {\nloop="$(losetup --find)"\n}\n' > "$planted"
[ "$(order_of "$planted")" = "no udevadm settle" ] \
    && ok "a helper with neither is refused" \
    || fail "a helper with neither is refused: $(order_of "$planted")"

printf 'fn guest_script() {\nudevadm settle\nbtrfs device scan --forget >/dev/null 2>&1 || true\nloop="$(losetup --find)"\n}\n' > "$planted"
[ "$(order_of "$planted")" = "no checked btrfs device scan --forget" ] \
    && ok "a forget whose failure is ignored is refused" \
    || fail "a forget whose failure is ignored is refused: $(order_of "$planted")"

printf 'fn guest_script() {\nbtrfs device scan --forget >/dev/null\nudevadm settle\nloop="$(losetup --find)"\n}\n' > "$planted"
[ "$(order_of "$planted")" = "the forget runs ahead of the settle" ] \
    && ok "a forget ahead of the settle is refused" \
    || fail "a forget ahead of the settle is refused: $(order_of "$planted")"

printf 'fn guest_script() {\nudevadm settle\nloop="$(losetup --find)"\nbtrfs device scan --forget >/dev/null\n}\n' > "$planted"
[ "$(order_of "$planted")" = "the loop device is attached ahead of the forget" ] \
    && ok "an attach ahead of the forget is refused" \
    || fail "an attach ahead of the forget is refused: $(order_of "$planted")"

printf 'fn other() {\nudevadm settle\nbtrfs device scan --forget >/dev/null\n}\nfn guest_script() {\nloop="$(losetup --find)"\n}\n' > "$planted"
[ "$(order_of "$planted")" = "no udevadm settle" ] \
    && ok "a forget in another function does not count" \
    || fail "a forget in another function does not count: $(order_of "$planted")"

if [ "$fails" -gt 0 ]; then
    echo "FAIL  $fails kernel-helper-forget violation(s)" >&2
    exit 1
fi
echo "PASS  the kernel oracle helper settles and forgets before it attaches a copy"
