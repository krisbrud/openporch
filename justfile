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

# Run every example in docs/api.md and docs/security.md, and the quickstart,
# against a dev server, and check that docs/cli.md matches `porch` help.
docs:
    cargo test -p openporch-server --test docs
    cargo test -p openporch-cli -- quickstart cli_reference

# Regenerate docs/cli.md from `porch` help.
cli-docs:
    OPENPORCH_UPDATE_DOCS=1 cargo test -p openporch-cli cli_reference

# Test the OpenTofu provider, then check that OpenTofu loads a local build of it
# against a dev server. Needs `go` and `tofu`.
provider:
    cd provider && test -z "$(gofmt -l .)" && go vet ./... && go test ./...
    cargo build -p openporch-server --bin openporch
    provider/scripts/smoke.sh target/debug/openporch

# Check dependency licenses, advisories and sources.
deny:
    cargo deny check
