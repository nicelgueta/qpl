#!/usr/bin/env bash
# Bumps the [package].version in Cargo.toml (semver major/minor/patch) and
# refreshes Cargo.lock to match. Invoked via `make bump <major|minor|patch>`.
set -euo pipefail

level="${1:-}"
if [[ "$level" != "major" && "$level" != "minor" && "$level" != "patch" ]]; then
	echo "Usage: make bump <major|minor|patch>" >&2
	exit 1
fi

cd "$(dirname "$0")/.."

current=$(awk -F ' = ' '$1 ~ /^version/ { gsub(/["]/, "", $2); print $2; exit }' Cargo.toml)
IFS='.' read -r raw_major raw_minor raw_patch <<<"$current"

# A segment may carry a trailing pre-release marker (a/b), e.g. "0.1a.0" or
# "0.1.0a". Strip it from whichever segment has it and remember that the
# current version is a pre-release.
has_suffix=false
for part in "$raw_major" "$raw_minor" "$raw_patch"; do
	if [[ "$part" =~ [ab]$ ]]; then
		has_suffix=true
	fi
done
major="${raw_major%[ab]}"
minor="${raw_minor%[ab]}"
patch="${raw_patch%[ab]}"

case "$level" in
major)
	major=$((major + 1))
	minor=0
	patch=0
	;;
minor)
	minor=$((minor + 1))
	patch=0
	;;
patch)
	if [[ "$has_suffix" == false ]]; then
		patch=$((patch + 1))
	fi
	;;
esac

new="${major}.${minor}.${patch}"

sed -i.bak "0,/^version = \".*\"/s//version = \"${new}\"/" Cargo.toml
rm -f Cargo.toml.bak

cargo check --quiet

git add Cargo.toml Cargo.lock

echo "Bumped version: ${current} -> ${new}"
