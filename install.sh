#!/bin/sh
# Install blinker, the incremental Mach-O linker for Rust on Apple Silicon.
#
#     curl -fsSL https://raw.githubusercontent.com/blasrodri/blinker/master/install.sh | sh
#
# Downloads the latest release, verifies its checksum, and puts the binary
# somewhere on your PATH. It configures nothing: which projects use blinker is
# a decision this script does not make for you, and `blinker --blinker-try
# build` is how you find out whether you want it to.
#
# POSIX sh, because it is piped into whatever /bin/sh is.

set -eu

REPO="blasrodri/blinker"
ASSET="blinker-aarch64-apple-darwin.tar.gz"
BASE="https://github.com/$REPO/releases/latest/download"

say() { printf 'blinker: %s\n' "$1" >&2; }
die() { printf 'blinker: %s\n' "$1" >&2; exit 1; }

# Refused rather than attempted. blinker emits arm64 Mach-O and nothing else,
# so an x86 or Linux install would produce a linker that fails on its first
# link — later, and less clearly than here.
[ "$(uname -s)" = "Darwin" ] || die "blinker is macOS only (this is $(uname -s))"
[ "$(uname -m)" = "arm64" ] || die "blinker is Apple Silicon only (this is $(uname -m))"

# Where the binary goes. `~/.cargo/bin` first because anyone linking Rust has
# it, and it is already on PATH; the others are fallbacks in the order a
# machine without a Rust toolchain would expect.
if [ -n "${BLINKER_INSTALL_DIR:-}" ]; then
    DEST="$BLINKER_INSTALL_DIR"
elif [ -d "$HOME/.cargo/bin" ]; then
    DEST="$HOME/.cargo/bin"
elif [ -d "$HOME/.local/bin" ]; then
    DEST="$HOME/.local/bin"
else
    DEST="/usr/local/bin"
fi
mkdir -p "$DEST" 2>/dev/null || die "cannot create $DEST — set BLINKER_INSTALL_DIR"
[ -w "$DEST" ] || die "$DEST is not writable — set BLINKER_INSTALL_DIR to somewhere it is"

WORK="$(mktemp -d)"
# Deliberately unconditional: a partial download left in /tmp is a file a later
# run could pick up.
trap 'rm -rf "$WORK"' EXIT INT TERM

say "downloading $ASSET"
curl -fsSL "$BASE/$ASSET" -o "$WORK/$ASSET" || die "download failed — is there a release yet?"
curl -fsSL "$BASE/$ASSET.sha256" -o "$WORK/$ASSET.sha256" || die "no checksum published"

# Verified before anything is unpacked, and the whole point is that this is not
# optional: a linker is a program every build runs, and installing an unchecked
# one over a pipe is how a build machine is compromised.
say "verifying checksum"
( cd "$WORK" && shasum -a 256 -c "$ASSET.sha256" >/dev/null 2>&1 ) \
    || die "checksum mismatch — refusing to install"

tar -xzf "$WORK/$ASSET" -C "$WORK" || die "the archive could not be unpacked"
[ -f "$WORK/blinker" ] || die "the archive does not contain blinker"

# Replaced rather than written through: overwriting a running binary in place
# is how you get a linker that is half of two versions.
chmod +x "$WORK/blinker"
mv -f "$WORK/blinker" "$DEST/blinker"

say "installed $("$DEST/blinker" --blinker-version) to $DEST/blinker"

case ":$PATH:" in
    *":$DEST:"*) ;;
    *) say "note: $DEST is not on your PATH" ;;
esac

cat >&2 <<'NEXT'

Next, from a Rust project of yours:

    blinker --blinker-try build     # build through blinker, changing nothing
    blinker --blinker-install       # keep it: writes .cargo/config.toml
    blinker --blinker-uninstall     # and back again
NEXT
