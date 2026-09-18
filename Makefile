.PHONY: build release test run wasm bump book book-build book-deploy vscode-ext

build:
	cargo build

release:
	cargo build --release

test:
	cargo test

run:
	cargo run

# Build the browser bundle into tools/wasm/pkg. Prepares a patched Polars
# checkout first (stock Polars doesn't compile for wasm32-unknown-unknown --
# see tools/wasm/README.md) and points cargo at it for the duration of the
# build. Slow on a cold cache: the whole Polars tree, for a new target.
wasm:
	@./scripts/build-wasm.sh

# Repackage the VSCode extension (tools/vscode) into a .vsix and install it,
# replacing whatever version is currently loaded. Reload the VSCode window
# afterwards to pick up the change — grammar/vocabulary edits aren't hot-reloaded.
vscode-ext:
	cd tools/vscode && rm -f *.vsix && npx vsce package && code --install-extension "$$(ls *.vsix)"

# Serve the mdbook locally and open it in a browser.
book:
	mdbook serve --open book

# Build the mdbook to book/book/.
book-build:
	mdbook build book

# Build the mdbook and copy it into ../nicelgueta.github.io/qpl. Leaves the
# result unstaged there for manual review/commit.
book-deploy:
	@./scripts/deploy-book.sh

# Bump the version in Cargo.toml, e.g. `make bump patch`.
bump:
	@./scripts/bump-version.sh $(filter-out $@,$(MAKECMDGOALS))

# Swallow the level argument (major/minor/patch) so make doesn't try to
# treat it as a target of its own.
%:
	@:
