#!/usr/bin/env bash
# Print a .qplc file's bytecode (or a .qpl script's, compiled on the fly) as text.
# Usage: scripts/qplc-dump.sh <file.qplc|file.qpl>
set -euo pipefail
[ $# -eq 1 ] || { echo "usage: $0 <file.qplc|file.qpl>" >&2; exit 2; }
cd "$(dirname "$0")/.."
exec cargo run --quiet -- -d "$(realpath "$1")"
