.PHONY: all build build-release check test test-verbose fmt fmt-check lint lint-fix doc ci bump-patch bump-minor bump-major clean help

# Default target
all: build

## Build the library (debug)
build:
	cargo build

## Build the library (release)
build-release:
	cargo build --release

## Run cargo check without producing build artifacts
check:
	cargo check --all-targets

## Run every test: unit, integration, doctests
test:
	cargo test --all-targets
	cargo test --doc


## Run the tests and show test output (TEST=name filters by test name)
test-verbose:
	cargo test --all-targets $(TEST) -- --nocapture

## Format the code
fmt:
	cargo fmt --all

## Check formatting without changing files (used in CI)
fmt-check:
	cargo fmt --all -- --check

## Run clippy, failing on any warning (used in CI)
lint:
	cargo clippy --all-targets -- -D warnings

## Run clippy and apply its suggested fixes
lint-fix:
	cargo clippy --all-targets --fix --allow-dirty

## Build the API docs, failing on broken intra-doc links
doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --no-deps

## Run everything CI runs: format check, lint, tests, docs
ci: fmt-check lint test doc

## Bump the version for a PR (every PR into main must; the merge is tagged with it)
bump-patch:
	scripts/bump-version.sh patch

## Bump the minor version (new features)
bump-minor:
	scripts/bump-version.sh minor

## Bump the major version (breaking changes, once past 0.x)
bump-major:
	scripts/bump-version.sh major

## Remove build artifacts
clean:
	cargo clean

## Show available targets
help:
	@echo ""
	@echo "Usage: make <target>"
	@echo ""
	@echo "Targets:"
	@grep -E '^## ' Makefile | sed 's/^## /  /'
	@echo ""
