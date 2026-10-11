# Development tasks for openporch. Run `just --list` to see them.

# Run the openporch server locally.
dev *args:
    cargo run -p openporch-server --bin openporch -- {{args}}

# Run formatting, lints and the full unit test suite, as CI does.
test:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

# Run end-to-end tests. They are marked #[ignore] so plain `cargo test` skips them.
e2e:
    cargo test --workspace -- --ignored

# Run every example in docs/api.md and docs/security.md against a dev server.
docs:
    cargo test -p openporch-server --test docs

# Check dependency licenses, advisories and sources.
deny:
    cargo deny check
