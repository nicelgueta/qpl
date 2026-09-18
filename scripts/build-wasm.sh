#!/usr/bin/env bash
# Builds the browser bundle (tools/wasm/pkg) in one shot. Invoked via `make wasm`.
#
# Stock Polars does not compile for wasm32-unknown-unknown (see
# tools/wasm/README.md for why), so this prepares a patched Polars checkout and
# points cargo at it for the duration of the build. Everything it creates
# outside the repo is idempotent — re-running is cheap once the checkout exists.
#
# Overridable:
#   POLARS_REPO      a polars clone to take the worktree from  (default ../polars)
#   POLARS_WASM_SRC  where the patched worktree lives          (default ../polars-wasm-<ver>)
#   WASM_PACK_ARGS   extra args for wasm-pack, e.g. --dev
set -euo pipefail

cd "$(dirname "$0")/.."
repo=$PWD

# The Polars version to patch is whatever Cargo.toml pins, so a polars bump
# surfaces here as a missing patch file rather than a silently stale checkout.
ver=$(awk -F'"' '/^polars = \{ version = / { print $2; exit }' Cargo.toml)
if [[ -z "$ver" ]]; then
	echo "build-wasm: could not read the polars version from Cargo.toml" >&2
	exit 1
fi

patch_file=$repo/tools/wasm/patches/polars-$ver-wasm32-unknown-unknown.patch
if [[ ! -f "$patch_file" ]]; then
	echo "build-wasm: no patch for polars $ver at $patch_file" >&2
	echo "  Polars was bumped without regenerating the wasm patch — rebase it onto" >&2
	echo "  the rs-$ver tag and save it under that name. See tools/wasm/README.md." >&2
	exit 1
fi

polars_repo=${POLARS_REPO:-$repo/../polars}
polars_src=${POLARS_WASM_SRC:-$repo/../polars-wasm-$ver}

# --- 1. a polars clone to take the worktree from -----------------------------
if [[ ! -d "$polars_repo/.git" ]]; then
	echo "==> cloning polars into $polars_repo (blobless, this takes a minute)"
	git clone --filter=blob:none https://github.com/pola-rs/polars "$polars_repo"
fi
if ! git -C "$polars_repo" rev-parse -q --verify "refs/tags/rs-$ver" >/dev/null; then
	echo "==> fetching tag rs-$ver"
	git -C "$polars_repo" fetch --tags origin
fi

# --- 2. a detached worktree at that tag, with the patch applied --------------
if [[ ! -d "$polars_src" ]]; then
	echo "==> checking out polars rs-$ver into $polars_src"
	git -C "$polars_repo" worktree add --detach "$polars_src" "rs-$ver"
fi
# `--reverse --check` succeeds only when the patch is *already* applied, which
# is how a re-run tells "nothing to do" from "dirty in some other way".
if git -C "$polars_src" apply --reverse --check "$patch_file" 2>/dev/null; then
	echo "==> polars patch already applied"
else
	echo "==> applying $(basename "$patch_file")"
	git -C "$polars_src" apply "$patch_file"
fi

# --- 3. toolchain ------------------------------------------------------------
rustup target add wasm32-unknown-unknown >/dev/null
if ! command -v wasm-pack >/dev/null; then
	echo "==> installing wasm-pack"
	cargo install wasm-pack
fi

# --- 4. redirect every polars crate at the patched checkout ------------------
# Every one of them, not just the four the patch touches: a patched crate
# resolves its siblings by path, so leaving the rest on crates.io puts two
# copies of polars-arrow in the graph and the build dies in a wall of
# "expected PlSmallStr, found PlSmallStr" type mismatches.
#
# The paths are machine-specific, so the block is appended for the duration of
# the build and taken off again afterwards (Cargo.lock too — the patch makes
# cargo rewrite it).
backup=$(mktemp -d)
cp Cargo.toml "$backup/Cargo.toml"
cp Cargo.lock "$backup/Cargo.lock"

restore() {
	mv -f "$backup/Cargo.toml" "$repo/Cargo.toml" || true
	mv -f "$backup/Cargo.lock" "$repo/Cargo.lock" || true
	rmdir "$backup" 2>/dev/null || true
}
# INT/TERM as well as EXIT: a ^C partway through a long build must not leave the
# machine-specific [patch.crates-io] block behind in Cargo.toml.
trap restore EXIT INT TERM

{
	echo
	echo "[patch.crates-io]"
	sed -n 's/^name = "\(polars[a-z0-9-]*\)"$/\1/p' Cargo.lock | sort -u | while read -r crate; do
		# polars-arrow-format / polars-parquet-format are published separately and
		# have no crates/ dir here, so skip whatever the checkout doesn't carry.
		if [[ -d "$polars_src/crates/$crate" ]]; then
			echo "$crate = { path = \"$polars_src/crates/$crate\" }"
		fi
	done
} >>Cargo.toml

# --- 5. build ----------------------------------------------------------------
# `getrandom_backend` is required on this target — polars' own `make -C crates
# check-wasm` sets the same flag.
echo "==> wasm-pack build (this is slow: the whole polars tree, for a new target)"
RUSTFLAGS='--cfg getrandom_backend="wasm_js"' \
	wasm-pack build --target web --out-dir tools/wasm/pkg \
	--no-default-features --features wasm ${WASM_PACK_ARGS:-}

echo
echo "==> done: tools/wasm/pkg"
ls -la tools/wasm/pkg
