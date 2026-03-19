.PHONY: quick check

quick:
	cargo check

check:
	cargo fmt --check
	cargo clippy --workspace -- -D warnings
	cargo test --workspace
