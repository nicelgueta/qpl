#!/usr/bin/env bash
# Copies the built mdbook (book/book/) into the sibling nicelgueta.github.io
# checkout's /qpl directory. Invoked via `make book-deploy`. Only copies
# files — never touches git state in the site repo, so changes can be
# reviewed and committed by hand.
set -euo pipefail

cd "$(dirname "$0")/.."

site_dir="../nicelgueta.github.io"
if [[ ! -d "$site_dir/.git" ]]; then
	echo "error: $site_dir is not a git checkout — expected it alongside this repo" >&2
	exit 1
fi

mdbook build book

dest="$site_dir/qpl"
# Fully replaced each time — everything under here is regenerated build
# output, never hand-edited.
rm -rf "$dest"
mkdir -p "$dest"
cp -a book/book/. "$dest/"

echo "Copied book/book/ -> $dest"
cd "$site_dir" && git status --short -- qpl
