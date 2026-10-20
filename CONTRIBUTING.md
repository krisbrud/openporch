# Contributing to openporch

Thank you for helping. openporch is licensed under the
[Apache License 2.0](LICENSE). By contributing you agree that your
contribution is licensed under it too.

## What to work on

The specification is the OpenSpec change in
[`openspec/changes/initial-platform-orchestrator-spec`](openspec/changes/initial-platform-orchestrator-spec),
and [`tasks.md`](openspec/changes/initial-platform-orchestrator-spec/tasks.md)
lists what is done and what is next. A change that adds behavior should match
a requirement and its scenarios there, or propose one. Open an issue first for
anything large.

## Rules the code follows

- `crates/core` stays pure: no I/O, no async, no dependency on other workspace
  crates.
- Secret values never reach the planner, logs or API responses. They travel as
  references and are resolved only in the executor.
- Every HTTP route declares the permission it needs.
- Input from users, drivers or the network is never `unwrap`ped.
- Do not copy code from Humanitec repositories. Inspiration from their
  Apache-2.0 examples is reimplemented.
- New dependencies need a license in the `deny.toml` allow-list.

## Building and testing

You need the Rust toolchain pinned in `rust-toolchain.toml`; rustup installs
it. [`just`](https://github.com/casey/just) runs the common tasks:

```sh
just test      # cargo fmt --check, clippy -D warnings and the unit tests, as CI runs them
just deny      # dependency licenses and advisories (needs cargo-deny)
just docs      # the documented examples and the quickstart against a dev server
just e2e       # tests marked #[ignore]; some need tofu or an AWS endpoint
just provider  # the OpenTofu provider (needs Go and tofu)
```

Run `just test` and `just deny` before you open a pull request. Changes to the
provider need `just provider`. Changes to a published schema regenerate it with
`OPENPORCH_UPDATE_SCHEMAS=1 cargo test -p openporch-core`, and `porch` help
changes regenerate [docs/cli.md](docs/cli.md) with `just cli-docs`. A database
migration needs a schema fixture, as
[`crates/server/fixtures/schema`](crates/server/fixtures/schema/README.md)
describes.

## Pull requests

Keep a pull request to one change, with tests that show it working. Describe
what a user or operator sees before and after. Update the documentation the
change affects, and [compatibility](docs/compatibility.md) when a contract or
persisted format changes.

## Security issues

Do not open a public issue for a vulnerability. [SECURITY.md](SECURITY.md)
says how to report one.
