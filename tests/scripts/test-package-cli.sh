#!/usr/bin/env bash
# The release tarball is an install prefix an installer copies as-is --
# bin/rust-fs-btrfs, bin/fs.btrfs as a relative symlink to it, a man page
# and three completions per name, share/rust-fs-btrfs/CAVEATS and the
# licence -- and every name in it runs and identifies itself.
#
# This runs the real packaging script, from a sandbox copy of the
# repository, against stand-in multi-call binaries: one that behaves, and
# one for each way a build can be wrong. The release workflow and the `cli`
# CI job run the same script against the real binary, so the checks here
# are the checks a release makes.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0
ok()   { echo "ok $*"; }
fail() { echo "not ok $*" >&2; fails=$((fails + 1)); }

mkdir -p "$ROOT/tmp"
sandbox="$(mktemp -d "$ROOT/tmp/package-cli-test.XXXXXX")"
trap 'rm -rf "$sandbox"' EXIT

# A repository of our own, so CAVEATS can be made wrong without touching
# the real one.
repo_copy="$sandbox/repo"
mkdir -p "$repo_copy/scripts" "$repo_copy/packaging"
cp "$ROOT/scripts/package-cli.sh" "$repo_copy/scripts/"
cp "$ROOT/Cargo.toml" "$ROOT/LICENSE" "$repo_copy/"
cp "$ROOT/packaging/CAVEATS" "$repo_copy/packaging/"
PACKAGE="$repo_copy/scripts/package-cli.sh"

crate="$(sed -n 's/^name = "\(.*\)"$/\1/p' "$ROOT/Cargo.toml" | head -n 1)"
[ "$crate" = "am-fs-btrfs" ] && ok "the crate name is read from Cargo.toml" \
    || fail "the crate name is read from Cargo.toml: '$crate'"

# stub DIR VERSION HELP-STATUS NAMES [NO-MAN-FOR] [NO-COMPLETIONS-FOR]:
# a stand-in rust-fs-btrfs in DIR that dispatches on its own name the way
# the real one does.
stub() {
    local dir="$sandbox/$1"
    mkdir -p "$dir"
    cat >"$dir/rust-fs-btrfs" <<STUB
#!/usr/bin/env bash
name="\$(basename "\$0")"
case "\${1:-}" in
    --help) echo "Usage: \$name"; exit $3 ;;
    --version) echo "\$name ($crate) $2" ;;
    generate)
        case "\$2" in
            names) printf '%s\n' $4 ;;
            man)
                mkdir -p "\$3/man/man1"
                for n in $4 rust-fs-btrfs; do
                    [ "\$n" = "${5:-}" ] || echo ".TH \$n 1" >"\$3/man/man1/\$n.1"
                done ;;
            completions)
                mkdir -p "\$3/zsh/site-functions" "\$3/bash-completion/completions" "\$3/fish/vendor_completions.d"
                for n in $4 rust-fs-btrfs; do
                    [ "\$n" = "${6:-}" ] && continue
                    echo "#compdef \$n" >"\$3/zsh/site-functions/_\$n"
                    echo "complete -F _\$n \$n" >"\$3/bash-completion/completions/\$n"
                    echo "complete -c \$n" >"\$3/fish/vendor_completions.d/\$n.fish"
                done ;;
        esac ;;
    *) exit 2 ;;
esac
STUB
    chmod +x "$dir/rust-fs-btrfs"
    printf '%s\n' "$dir"
}

# package VERSION LABEL TARGET-DIR: run the script in a fresh output
# directory and print the tarball's absolute path. STALE=<name> first leaves
# a file of that name there, as a previous run would have.
package() {
    local out="$sandbox/out-$RANDOM$RANDOM" name status
    mkdir -p "$out"
    printf '%s\n' "$out" >"$sandbox/package-out"
    [ -z "${STALE:-}" ] || echo stale >"$out/$STALE"
    name="$(cd "$out" && bash "$PACKAGE" "$@" 2>"$sandbox/stderr")"
    status=$?
    if [ "$status" -ne 0 ]; then
        printf '%s' "$name"
        return "$status"
    fi
    printf '%s\n' "$out/$name"
}

# --- A good build: the tarball, its name, and exactly its contents. -------
good="$(stub good 9.9.9 0 fs.btrfs)"
if tarball="$(package 9.9.9 linux-x86_64 "$good")"; then
    ok "a good build packages"
else
    fail "a good build packages: $(cat "$sandbox/stderr")"
    tarball=""
fi
[ "$(basename "$tarball")" = "$crate-9.9.9-linux-x86_64.tar.gz" ] \
    && ok "the tarball is <crate>-<version>-<label>.tar.gz" \
    || fail "the tarball is <crate>-<version>-<label>.tar.gz, got '$tarball'"

# The content checks need the tarball: without it they fail rather than
# fall silent, since a check that does not run reads like one that passed.
if [ -f "$tarball" ]; then
    ok "the packaged tarball exists"
    # LC_ALL=C because `want` below is written in byte order, LICENSE before
    # bin/. A bare `sort` collates by the caller's locale, and en_GB/en_US
    # put LICENSE after bin/, so the check failed on a correct tarball
    # everywhere but a C-locale CI runner (#237).
    files="$(tar -tzf "$tarball" | sed 's|^\./||' | grep -v '/$' | LC_ALL=C sort | tr '\n' ' ')"
    want="LICENSE bin/fs.btrfs bin/rust-fs-btrfs share/bash-completion/completions/fs.btrfs share/bash-completion/completions/rust-fs-btrfs share/fish/vendor_completions.d/fs.btrfs.fish share/fish/vendor_completions.d/rust-fs-btrfs.fish share/man/man1/fs.btrfs.1 share/man/man1/rust-fs-btrfs.1 share/rust-fs-btrfs/CAVEATS share/zsh/site-functions/_fs.btrfs share/zsh/site-functions/_rust-fs-btrfs "
    [ "$files" = "$want" ] && ok "the tarball holds exactly the install prefix" \
        || fail "the tarball holds exactly the install prefix, got: $files"
    unpacked="$sandbox/unpacked"
    mkdir -p "$unpacked"
    tar -xzf "$tarball" -C "$unpacked"
    [ -L "$unpacked/bin/fs.btrfs" ] && [ "$(readlink "$unpacked/bin/fs.btrfs")" = rust-fs-btrfs ] \
        && ok "bin/fs.btrfs is a relative symlink to rust-fs-btrfs" \
        || fail "bin/fs.btrfs is a relative symlink to rust-fs-btrfs"
    cmp -s "$unpacked/bin/rust-fs-btrfs" "$good/rust-fs-btrfs" \
        && ok "bin/rust-fs-btrfs is the built binary" || fail "bin/rust-fs-btrfs is the built binary"
    cmp -s "$unpacked/share/rust-fs-btrfs/CAVEATS" "$ROOT/packaging/CAVEATS" \
        && ok "share/rust-fs-btrfs/CAVEATS is packaging/CAVEATS" \
        || fail "share/rust-fs-btrfs/CAVEATS is packaging/CAVEATS"
    [ "$(wc -l <"$ROOT/packaging/CAVEATS" | tr -d ' ')" -le 4 ] \
        && ok "packaging/CAVEATS is at most four lines" || fail "packaging/CAVEATS is at most four lines"
else
    fail "the packaged tarball exists at '$tarball'"
fi

# --- Each way a build can be wrong is refused, with no tarball left. ------
# refused WHY PATTERN VERSION LABEL TARGET-DIR: packaging fails, saying
# PATTERN, and leaves no tarball.
refused() {
    local why="$1" pattern="$2"
    shift 2
    local stdout out_dir left
    if stdout="$(package "$@")"; then
        fail "$why is refused, but packaging succeeded: $stdout"
        return
    fi
    out_dir="$(cat "$sandbox/package-out")"
    left="$(find "$out_dir" -maxdepth 1 -name '*.tar.gz')"
    if [ -n "$stdout" ]; then
        fail "$why names a tarball on stdout: $stdout"
    elif [ -n "$left" ]; then
        fail "$why leaves a tarball behind: $left"
    elif ! grep -qE -e "$pattern" "$sandbox/stderr"; then
        fail "$why is refused for another reason than /$pattern/: $(cat "$sandbox/stderr")"
    else
        ok "$why is refused, and leaves no tarball"
    fi
}

refused "a missing binary" "no built rust-fs-btrfs" 9.9.9 linux-x86_64 "$sandbox/nowhere"
refused "a binary whose --help fails" "--help failed" 9.9.9 linux-x86_64 "$(stub helpfails 9.9.9 1 fs.btrfs)"
refused "a binary reporting a version other than the tag's" "--version says" 9.9.9 linux-x86_64 "$(stub wrongver 1.0.0 0 fs.btrfs)"
refused "a binary that lists no tool" "listed no tool" 9.9.9 linux-x86_64 "$(stub nonames 9.9.9 0 '')"
refused "a tool name that is a path" "not a tool name" 9.9.9 linux-x86_64 "$(stub pathname 9.9.9 0 ../fs.btrfs)"
refused "a name with no man page" "no man page for fs.btrfs" 9.9.9 linux-x86_64 "$(stub noman 9.9.9 0 fs.btrfs fs.btrfs)"
refused "a name with no completions" "no share/zsh/site-functions/_fs.btrfs" 9.9.9 linux-x86_64 "$(stub nocomp 9.9.9 0 fs.btrfs '' fs.btrfs)"
refused "a missing label" "usage" 9.9.9 "" "$good"
refused "a missing version" "usage" "" linux-x86_64 "$good"
STALE="$crate-9.9.9-linux-x86_64.tar.gz" refused "a failure beside a previous run's tarball" "no built rust-fs-btrfs" 9.9.9 linux-x86_64 "$sandbox/nowhere"
printf '%s\n' one two three four five >"$repo_copy/packaging/CAVEATS"
refused "a CAVEATS of five lines" "CAVEATS is 5 lines" 9.9.9 linux-x86_64 "$good"
cp "$ROOT/packaging/CAVEATS" "$repo_copy/packaging/CAVEATS"

if [ "$fails" -gt 0 ]; then
    echo "FAIL  $fails packaging check(s)" >&2
    exit 1
fi
echo "PASS  the release tarball is an install prefix whose every name runs and names this crate"
