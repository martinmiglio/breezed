# Developer tasks. Run from the repository root.

default: ci

# Format the workspace.
fmt:
    cargo fmt --all

# Fail if formatting differs.
fmt-check:
    cargo fmt --all -- --check

# Lint every target with warnings denied.
lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Run the full test suite.
test:
    cargo test --all-features

# Fast type-check.
check:
    cargo check --all-targets --all-features

# Build rustdoc with warnings denied.
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps

# Everything CI runs.
ci: fmt-check lint test doc

# Run the CLI from the debug build.
run *args:
    cargo run -- {{args}}

# Build the release binary.
build:
    cargo build --release
