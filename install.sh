#!/bin/sh
# Install blinker, the incremental Mach-O linker for Rust on Apple Silicon.
#
#     curl -fsSL https://raw.githubusercontent.com/blasrodri/blinker/master/install.sh | sh
#     curl -fsSL https://.../install.sh | sh -s -- --use    # and set it up here
#
# Downloads the latest release, verifies its checksum, and puts the binary
# somewhere on your PATH.
#
# With no arguments it configures nothing: which projects use blinker is a
# decision this script does not make for you, and `blinker --blinker-try build`
# is how you find out whether you want it to. `--use` says you have already
# decided, and runs `--blinker-install` in the directory you ran this from —
# which is the whole of the setup, so that it can be one line.
#
# POSIX sh, because it is piped into whatever /bin/sh is.

set -eu

# Parsed before anything is downloaded, so a typo fails now rather than after
# a binary has been put on the machine.
USE_HERE=""
for arg in "$@"; do
    case "$arg" in
        --use) USE_HERE=1 ;;
        -h|--help)
            sed -n '2,15p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            printf 'blinker: unknown option %s (try --use)\n' "$arg" >&2
            exit 1
            ;;
    esac
done

REPO="blasrodri/blinker"
ASSET="blinker-aarch64-apple-darwin.tar.gz"
# Overridable so this script can be tested end to end against a `file://`
# release rather than only in production, on a user's machine, once. Nothing a
# user sets: if it is wrong the checksum below still has to match.
BASE="${BLINKER_RELEASE_BASE:-https://github.com/$REPO/releases/latest/download}"

say() { printf 'blinker: %s\n' "$1" >&2; }
die() { printf 'blinker: %s\n' "$1" >&2; exit 1; }

# Before the download, not after it. `--use` says "and set it up here", so if
# there is no project here the answer is known already — and finding out after
# a binary has been put on the machine is a worse version of the same message.
if [ -n "$USE_HERE" ] && [ ! -f Cargo.toml ]; then
    die "no Cargo.toml here — cd to a project, or drop --use to just install the binary"
fi

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

if [ -n "$USE_HERE" ]; then
    # In the directory the user ran this from, which for a piped script is the
    # one they are standing in. Checked for a project up at the top, before
    # anything was downloaded.
    say "setting blinker as the linker for $(pwd)"
    "$DEST/blinker" --blinker-install || die "setup failed; the binary is installed and works"
    cat >&2 <<'BUILT'

Done — `cargo build` now links with blinker. To undo it:

    blinker --blinker-uninstall     # removes the key, and the file if it held nothing else
BUILT
else
    cat >&2 <<'NEXT'

Next, from a Rust project of yours:

    blinker --blinker-try build     # build through blinker, changing nothing
    blinker --blinker-install       # keep it: writes .cargo/config.toml
    blinker --blinker-uninstall     # and back again
NEXT
fi
