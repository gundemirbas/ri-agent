preflight:
	cargo fmt --all -- --check
	cargo clippy --all-targets --all-features --quiet -- -D warnings
	# Build bins first so the sandbox unit test can spawn the ri-sandbox child.
	cargo build --bins --quiet
	cargo test --all-features --quiet
	cargo check --all-targets --all-features --quiet

# Provision a static uutils coreutils into the sandbox staging (optional but
# recommended — gives the sandbox self-contained file tools). See
# docs/CONTAINER-RUNTIME-SPEC.md §15.
sandbox-provision:
	./scripts/fetch-uutils-coreutils.sh

# Build both binaries and run just the sandbox tests (runtime + tool path).
sandbox-test:
	cargo build --bins --quiet
	cargo test --all-features sandbox --quiet
