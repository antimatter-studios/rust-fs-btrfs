#!/usr/bin/env bash
# package-cli.sh <version> <label> [target-dir]
#
# Package the built command-line tools as a release tarball in the current
# directory, check it, and print its file name on stdout.
#
#   <version>     the release version, without the leading `v`
#   <label>       the platform, e.g. darwin-arm64 or linux-x86_64
#   [target-dir]  where cargo put the release build (default: target/release)
#
# THE TARBALL IS THE CONTRACT with whatever installs it, and it is an install
# prefix, so an installer copies it whole and needs to know nothing about
# which tools are in it:
#
#   bin/<repo>                              the multi-call binary, the real file
#   bin/<dotted name>                       -> <repo>, a relative symlink, per tool
#   share/man/man1/...                      a page per name and per subcommand
#   share/zsh/site-functions/_<name>        a completion per name, per shell
#   share/bash-completion/completions/<name>
#   share/fish/vendor_completions.d/<name>.fish
#   share/<repo>/CAVEATS                    at most four lines, shown after install
#   LICENSE
#
# <repo> is the repository's name, from Cargo.toml's `repository`. The dotted
# names, the man pages and the completions all come from the binary itself
# (`<repo> generate names|man|completions`), so this script names no tool
# and the documentation cannot describe a flag the program does not take.
# Cargo refuses a dot in a target name, so the dotted names are made here,
# and nothing cargo calls anything appears in the tarball.
#
# THEN IT CHECKS WHAT IT BUILT, because a tarball whose tools do not run is
# worse than no tarball: the failure would surface as a user's bug report
# rather than a red build. The member list must be exactly the staged one,
# every dotted name a relative symlink to bin/<repo>, every name a man page
# and three completions, CAVEATS at most four lines, and every name must
# answer --help and report `<name> (<crate>) <version>` from --version --
# which identifies it among same-named tools from other packages and catches
# a tag that disagrees with Cargo.toml. On any failure nothing is printed on
# stdout and no tarball is left behind. tests/scripts/test-package-cli.sh
# holds this script to all of that.
set -euo pipefail

version="${1:-}"
label="${2:-}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target_dir="${3:-$root/target/release}"
licences=(LICENSE)

die() { echo "package-cli: $*" >&2; exit 1; }

[ -n "$version" ] || die "usage: package-cli.sh <version> <label> [target-dir]"
[ -n "$label" ] || die "usage: package-cli.sh <version> <label> [target-dir]"

crate="$(sed -n 's/^name = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)"
[ -n "$crate" ] || die "no package name in $root/Cargo.toml"
repo="$(sed -n 's/^repository = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)"
repo="${repo%/}"
repo="${repo##*/}"
[ -n "$repo" ] || die "no repository in $root/Cargo.toml"

tarball="$crate-$version-$label.tar.gz"
work="$(mktemp -d)"

# ON ANY FAILURE, NO TARBALL: not a partial one, and not one a previous run
# left under the same name, which a caller could otherwise take for this
# run's output.
cleanup() {
    local status=$?
    rm -rf "$work"
    [ "$status" -eq 0 ] || rm -f "$tarball"
    return "$status"
}
trap cleanup EXIT
rm -f "$tarball"

built="$target_dir/$repo"
[ -x "$built" ] || die "no built $repo at $built (cargo build --release --locked --features cli --bin $repo)"

stage="$work/stage"
mkdir -p "$stage/bin" "$stage/share/$repo" "$work/unpacked"
cp "$built" "$stage/bin/$repo"
chmod 755 "$stage/bin/$repo"
names="$("$stage/bin/$repo" generate names)" || die "$repo generate names failed"
[ -n "$names" ] || die "$repo generate names listed no tool"
for name in $names; do
    case "$name" in
        */* | .* | "$repo") die "$repo generate names listed '$name', which is not a tool name" ;;
    esac
    ln -s "$repo" "$stage/bin/$name"
done
"$stage/bin/$repo" generate man "$stage/share" >/dev/null || die "$repo generate man failed"
"$stage/bin/$repo" generate completions "$stage/share" >/dev/null || die "$repo generate completions failed"
cp "$root/packaging/CAVEATS" "$stage/share/$repo/CAVEATS"
for f in "${licences[@]}"; do
    cp "$root/$f" "$stage/$f"
done

# What was staged is what the tarball must hold: files and symlinks, not
# directories, whose listing varies by tar.
want_list="$(cd "$stage" && find . \( -type f -o -type l \) | sed 's|^\./||' | sort)"

# COPYFILE_DISABLE keeps macOS tar from adding ._ AppleDouble members.
COPYFILE_DISABLE=1 tar -czf "$tarball" -C "$stage" bin share "${licences[@]}"

got_list="$(tar -tzf "$tarball" | sed 's|^\./||' | grep -v '/$' | sort)"
[ "$got_list" = "$want_list" ] \
    || die "$tarball holds [$(echo $got_list)], expected [$(echo $want_list)]"

u="$work/unpacked"
tar -xzf "$tarball" -C "$u"

# THE LAYOUT, checked on what came out of the tarball.
[ -f "$u/bin/$repo" ] && [ ! -L "$u/bin/$repo" ] || die "bin/$repo is not a regular file in $tarball"
[ -x "$u/bin/$repo" ] || die "bin/$repo is not executable in $tarball"
for name in $names "$repo"; do
    exe="$u/bin/$name"
    if [ "$name" != "$repo" ]; then
        [ -L "$exe" ] || die "bin/$name is not a symlink in $tarball"
        [ "$(readlink "$exe")" = "$repo" ] \
            || die "bin/$name points at '$(readlink "$exe")', not the relative '$repo'"
    fi
    page="$u/share/man/man1/$name.1"
    case "$name" in mkfs.* | fsck.*) page="$u/share/man/man8/$name.8" ;; esac
    [ -s "$page" ] || die "no man page for $name (${page#"$u"/})"
    for completion in "share/zsh/site-functions/_$name" \
                      "share/bash-completion/completions/$name" \
                      "share/fish/vendor_completions.d/$name.fish"; do
        [ -s "$u/$completion" ] || die "no $completion"
    done
    "$exe" --help >/dev/null 2>&1 || die "$name --help failed"
    reported="$("$exe" --version 2>&1)" || die "$name --version failed"
    [ "$reported" = "$name ($crate) $version" ] \
        || die "$name --version says '$reported', expected '$name ($crate) $version'"
done
caveats="$u/share/$repo/CAVEATS"
[ -s "$caveats" ] || die "share/$repo/CAVEATS is empty"
lines="$(wc -l <"$caveats" | tr -d ' ')"
[ "$lines" -le 4 ] || die "share/$repo/CAVEATS is $lines lines; an installer prints it whole, and the most is four"
for f in "${licences[@]}"; do
    [ -s "$u/$f" ] || die "$f is empty in $tarball"
done

printf '%s\n' "$tarball"
