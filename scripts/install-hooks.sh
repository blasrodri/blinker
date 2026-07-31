#!/usr/bin/env bash
# Point git at the version-controlled hooks in `scripts/hooks`.
#
# `core.hooksPath` rather than copying into `.git/hooks`: a copy drifts from
# the version-controlled original the moment either changes, and nothing
# notices.
#
#   ./scripts/install-hooks.sh

set -euo pipefail

cd "$(dirname "$0")/.."

chmod +x scripts/hooks/*
git config core.hooksPath scripts/hooks

echo "core.hooksPath -> scripts/hooks"
echo "Hooks installed:"
ls -1 scripts/hooks | sed 's/^/  /'
