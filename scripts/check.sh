#!/usr/bin/env bash
# blinker's local gate.
#
# One command that answers "is this milestone's work safe to consider done?".
# There is no hosted CI: this runs on the developer's own Apple Silicon Mac,
# which is also the only supported target, so there is no environment gap.
#
# Run before every commit that claims to close out a deliverable.
#
#   ./scripts/check.sh          # full gate
#   ./scripts/check.sh --fast   # skip the slow end-to-end cargo builds

set -euo pipefail

cd "$(dirname "$0")/.."

FAST=0
[[ "${1:-}" == "--fast" ]] && FAST=1

step() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }

step "Platform check"
if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
  echo "blinker targets aarch64-apple-darwin and is developed on it." >&2
  echo "Detected $(uname -s)/$(uname -m); the gate cannot run here." >&2
  exit 1
fi
echo "$(uname -s)/$(uname -m) — ok"

step "Formatting"
cargo fmt --all --check

step "Lints"
cargo clippy --workspace --all-targets -- -D warnings

step "Unit and property tests"
if [[ $FAST -eq 1 ]]; then
  # --lib skips tests/, which is where the end-to-end cargo builds live.
  cargo test --workspace --lib
else
  # The full run includes integration tests that spawn real `cargo build`
  # invocations through the real blinker binary. These are what actually
  # establish the milestone acceptance criteria, so they are not optional
  # outside of --fast.
  cargo test --workspace
fi

printf '\n\033[1;32m==> gate passed\033[0m\n'
