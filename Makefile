.PHONY: build release test run bump book book-build book-deploy

build:
	cargo build

release:
	cargo build --release

test:
	cargo test

run:
	cargo run

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
