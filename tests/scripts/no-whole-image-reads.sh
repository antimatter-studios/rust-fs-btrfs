#!/usr/bin/env bash
# Whole-image reads make parallel tests proportional to disk size in RAM.
# Small outputs go through read_small_file, which enforces a 1 MiB cap.
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0
ok() { echo "ok $*"; }
fail() { echo "not ok $*" >&2; fails=$((fails + 1)); }
pattern='(^|[^[:alnum:]_])(std::)?fs::read[[:space:]]*\('
if printf '%s\n' 'let bytes = std::fs::read(&image);' | rg -q "$pattern"; then
    ok 'the guard detects a whole-image read'
else
    fail 'the guard detects a whole-image read'
fi
hits="$(rg -n "$pattern" "$REPO/tests" --glob '*.rs' || true)"
if [ -z "$hits" ]; then
    ok 'tests use bounded image reads and size-capped small-file reads'
else
    printf '%s\n' "$hits" >&2
    fail 'tests contain direct whole-file reads'
fi
# Explicit small-read allow-list. Each call goes through read_small_file's
# 1 MiB hard cap; a new call or a moved call needs its purpose reviewed here.
while IFS=: read -r source count; do
    source="${source#"$REPO/"}"
    case "$source" in
        tests/cli_dispatch.rs) expected=2 ;;          # literal sentinel file
        tests/cli_export_kernel.rs) expected=1 ;;     # literal destination sentinel
        tests/cli_export_tree.rs) expected=3 ;;       # tiny exported text payloads
        tests/cli_export_tree_kernel.rs) expected=3 ;; # tiny exported text/sentinel files
        tests/cli_xattr.rs) expected=1 ;;              # literal xattr destination sentinel
        tests/fuzz_decoders.rs) expected=1 ;;          # size-capped corpus seeds
        tests/oracle_streams.rs) expected=1 ;;         # missing stream's OS error
        tests/send_stream_kernel.rs) expected=3 ;;     # size-capped kernel send streams
        tests/super_write_oracle.rs) expected=1 ;;     # captured 4 KiB superblock
        tests/support/src/lib.rs) expected=2 ;;       # helper and cap regression test
        *) expected=0 ;;
    esac
    if [ "$count" -eq "$expected" ]; then
        ok "$source has $count reviewed small read(s)"
    else
        fail "$source has $count small read(s); allow-list expects $expected"
    fi
done < <(rg -c 'read_small_file\(' "$REPO/tests" --glob '*.rs' || true)
if [ "$(rg -c 'read_bounded_file\(' "$REPO/tests/support/src/oracle.rs" || true)" = 1 ]; then
    ok 'oracle output has one reviewed 16 MiB bounded read'
else
    fail 'oracle output bounded-read allow-list changed'
fi
[ "$fails" -eq 0 ] || exit 1
