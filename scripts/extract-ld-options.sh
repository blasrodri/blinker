#!/usr/bin/env bash
# Regenerate the ld64 option/arity table in crates/arguments/src/reference.rs.
#
# Arity — how many arguments an option consumes — is the property that causes
# silent misclassification when wrong, and it is not something to discover one
# project at a time. Two authoritative sources encode it:
#
#   1. Apple's `man ld` on this host, which documents each option with its
#      argument names (`-alias symbol_name alternate_symbol_name`), so arity is
#      mechanically extractable.
#   2. LLD's lld/MachO/Options.td, whose Separate/Joined/MultiArg declarations
#      say the same thing in machine-readable form and cover options Apple's
#      page omits.
#
# This script handles (1). The LLD-derived additions are maintained inline in
# the generator below; refresh them from
# https://raw.githubusercontent.com/llvm/llvm-project/main/lld/MachO/Options.td
# when the toolchain moves.
#
# Run after an Xcode/toolchain update, then re-run ./scripts/check.sh.

set -euo pipefail
cd "$(dirname "$0")/.."

command -v man >/dev/null || { echo "man not available" >&2; exit 1; }

man ld 2>/dev/null | col -b > /tmp/blinker-ld-man.txt
count=$(grep -cE '^     -[A-Za-z_]' /tmp/blinker-ld-man.txt || true)
if [[ "$count" -lt 100 ]]; then
  echo "man ld yielded only $count option lines — refusing to regenerate from it" >&2
  exit 1
fi

echo "Parsed $count option lines from man ld."
echo "Now update crates/arguments/src/reference.rs via the generator in this"
echo "script's history, or by hand for a small change, then run:"
echo "    cargo test -p blinker-arguments"
