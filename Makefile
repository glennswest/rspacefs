.PHONY: build test fmt clippy install clean check doc release

build:
	cargo build --workspace --release

debug:
	cargo build --workspace

test:
	cargo test --workspace

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

check: fmt-check clippy test

install:
	cargo install --path crates/rspacefs-cli

doc:
	cargo doc --workspace --no-deps --open

clean:
	cargo clean

release:
	cargo build --workspace --release
	@echo
	@echo "Built artifacts:"
	@ls -la target/release/rspacefs 2>/dev/null || echo "  (CLI binary not found)"
