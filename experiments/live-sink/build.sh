#!/usr/bin/env bash
# Build cg_clif with the Blinker Live output sink (§34).
#
# The pristine source is the toolchain's own — `rustc-src` ships
# `compiler/rustc_codegen_cranelift` — so nothing is vendored into this
# repository except the patch itself. That keeps the diff reviewable, which is
# the point: this is meant to be a narrow, plausibly upstreamable output-sink
# change, not a fork of a codegen backend.
#
#   ./build.sh [destination]
#
# Prints the path of the resulting dylib, which `spike --backend <path>` takes.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
toolchain=$(rustup toolchain list | awk '/nightly-2026-07-27/ {print $1; exit}')
if [ -z "${toolchain:-}" ]; then
  echo "the pinned nightly (nightly-2026-07-27) is not installed" >&2
  exit 1
fi
sysroot=$(rustup run "$toolchain" rustc --print sysroot)
source_tree="$sysroot/lib/rustlib/rustc-src/rust/compiler/rustc_codegen_cranelift"
if [ ! -d "$source_tree" ]; then
  echo "rustc-src is not installed: rustup component add rust-src --toolchain $toolchain" >&2
  exit 1
fi

destination=${1:-${TMPDIR:-/tmp}/blinker-live-sink}
rm -rf "$destination"
mkdir -p "$(dirname "$destination")"
cp -R "$source_tree" "$destination"
# cg_clif pins a nightly of its own, which is not the one this spike measures
# on. Removing it makes the build inherit the toolchain chosen above.
rm -f "$destination/rust-toolchain.toml"
patch -s -p1 -d "$destination" < "$here/live-sink.patch"

( cd "$destination" && cargo "+$toolchain" build --release )
echo "$destination/target/release/librustc_codegen_cranelift.dylib"
