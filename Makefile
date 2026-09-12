.PHONY: build release test run bump

build:
	cargo build

release:
	cargo build --release

test:
	cargo test

run:
	cargo run

# Bump the version in Cargo.toml, e.g. `make bump patch`.
bump:
	@./scripts/bump-version.sh $(filter-out $@,$(MAKECMDGOALS))

# Swallow the level argument (major/minor/patch) so make doesn't try to
# treat it as a target of its own.
%:
	@:
