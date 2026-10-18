#!/usr/bin/env bash
# Exercise the real host driver with a stub guest and BSD-style cp (#471).
# The payload is ordinary bytes, not a filesystem fixture; no VM is started.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0
ok()   { echo "ok $*"; }
fail() { echo "not ok $*" >&2; fails=$(( fails + 1 )); }

mkdir -p "$REPO/tmp"
sandbox="$(mktemp -d "$REPO/tmp/fixture-host-copy.XXXXXX")"
trap 'rm -rf "$sandbox"' EXIT
SUT="$sandbox/repository with spaces"
HARNESS="$sandbox/fs-linux-test-harness"
mkdir -p "$SUT/test-disks" "$HARNESS/scripts" "$sandbox/bin" "$sandbox/expected"
cp "$REPO/test-disks/build-fixtures.sh" "$SUT/test-disks/"
export COPY_SHARE="$sandbox/share" COPY_EXPECTED="$sandbox/expected"
export COPY_REAL_CP
COPY_REAL_CP="$(command -v cp)"

# Distinct bytes cross a copy-buffer boundary; one image ends in a hole,
# the other has data in its final block. Retain originals for cmp readback.
python3 - "$COPY_EXPECTED" <<'PY'
import pathlib, sys
root = pathlib.Path(sys.argv[1])
for name, tail in [('plain.img', b'tail'), ('nested/hole.img', b'')]:
    image = root / name
    image.parent.mkdir(parents=True, exist_ok=True)
    with image.open('wb') as stream:
        stream.seek(65536 + 0x40)
        stream.write(b'_BHRfS_M')
        stream.seek(1024 * 1024 - 2)
        stream.write(b'cross-boundary\x00\xff')
        stream.seek(8 * 1024 * 1024)
        stream.write(tail)
        stream.truncate(8 * 1024 * 1024 + 19)
(root / 'nested/hole.manifest').write_bytes(b'manifest\x00\xff\n')
PY

cat > "$sandbox/bin/cp" <<'STUB'
#!/usr/bin/env bash
for arg in "$@"; do
    case "$arg" in
        --sparse*) echo "cp: illegal option -- - (BSD stub)" >&2; exit 64 ;;
    esac
done
exec "$COPY_REAL_CP" "$@"
STUB
cat > "$HARNESS/scripts/vm.sh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
case "$1" in
    share) echo "$COPY_SHARE" ;;
    run)
        python3 - <<'PY'
import os, pathlib
source = pathlib.Path(os.environ['COPY_EXPECTED'])
output = pathlib.Path(os.environ['COPY_SHARE']) / 'fixtures'
for image in source.rglob('*'):
    if image.is_file():
        destination = output / image.relative_to(source)
        destination.parent.mkdir(parents=True, exist_ok=True)
        os.link(image, destination)
PY
        ;;
    *) exit 2 ;;
esac
STUB
printf '# Stub session: no VM is started.\n' > "$HARNESS/scripts/vm-session.sh"
chmod +x "$sandbox/bin/cp" "$HARNESS/scripts/vm.sh"

out="$(PATH="$sandbox/bin:$PATH" bash "$SUT/test-disks/build-fixtures.sh" geometry 2>&1)"; rc=$?
if [ "$rc" = 0 ]; then
    ok "the host copy succeeds with BSD-style cp"
else
    fail "the host copy fails with BSD-style cp (status $rc): $out"
fi
for image in plain.img nested/hole.img; do
    if cmp -s "$COPY_EXPECTED/$image" "$SUT/test-disks/$image"; then
        ok "$image is byte-identical after the host copy"
    else
        fail "$image differs after the host copy"
    fi
done
if cmp -s "$COPY_EXPECTED/nested/hole.manifest" "$SUT/test-disks/nested/hole.manifest"; then
    ok "non-image artefacts remain byte-identical"
else
    fail "non-image artefacts differ"
fi
if [ -z "$(find "$COPY_SHARE/fixtures" -type f -print)" ]; then
    ok "the host removes the installed guest artefacts"
else
    fail "guest artefacts remain after installation"
fi
if [ -z "$(find "$SUT/test-disks" -name '*.partial' -print)" ]; then
    ok "no partial copy remains after installation"
else
    fail "a partial copy remains after installation"
fi
if python3 - "$COPY_EXPECTED" "$SUT/test-disks" <<'PY'
import pathlib, sys
source, destination = map(pathlib.Path, sys.argv[1:])
for name in ['plain.img', 'nested/hole.img']:
    before = (source / name).stat()
    after = (destination / name).stat()
    # A host filesystem that allocates holes in the source cannot promise
    # sparse output. Where it does, the copied image must stay sparse too.
    if before.st_blocks * 512 < before.st_size // 2:
        assert after.st_blocks * 512 < after.st_size // 2, name
PY
then
    ok "the host keeps images sparse where the filesystem supports holes"
else
    fail "the host expands sparse images"
fi

[ "$fails" = 0 ] || exit 1
echo "PASS  the host installs byte-identical sparse images with BSD-style cp"
