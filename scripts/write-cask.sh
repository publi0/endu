#!/bin/sh
# Usage: scripts/write-cask.sh <version> <sha256> <owner/repo>
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
exec python3 "$root/scripts/release.py" write-cask "$@"
