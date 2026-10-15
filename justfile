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
    cd provider && { test -z "$(gofmt -l .)" || { gofmt -l .; exit 1; }; } && go vet ./... && go test ./...
    cargo build -p openporch-server --bin openporch
    provider/scripts/smoke.sh target/debug/openporch
    provider/scripts/acceptance.sh target/debug/openporch

# Check dependency licenses, advisories and sources.
deny:
    cargo deny check

# Apply examples/aws-platform against a dev server and an AWS endpoint, such as
# Floci at AWS_ENDPOINT_URL, then destroy it. Needs `go`, `tofu` and `jq`.
example-platform:
    cargo build -p openporch-server --bin openporch
    examples/aws-platform/test.sh target/debug/openporch

# The opt-in end-to-end test against a real AWS account, which creates billed
# resources and deletes them again. See docs/e2e.md for what it needs.
e2e-aws:
    cargo build -p openporch-server --bin openporch -p openporch-cli --bin porch
    scripts/e2e-aws.sh target/debug/openporch target/debug/porch
