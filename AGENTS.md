# Agent guidelines for openporch

openporch is a Rust platform orchestrator modeled on Humanitec's. The specification is the OpenSpec change in `openspec/changes/initial-platform-orchestrator-spec`; implementation follows `tasks.md` there, one task group per pull request.

## Working in this repository

- Run `just test` (or the commands it lists) before pushing. CI runs `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` and `cargo deny check`.
- `crates/core` must stay pure: no I/O, no async, no dependencies on other workspace crates.
- Secret values never reach the planner, logs or API responses. They travel as references and are resolved only in the executor.
- Every HTTP route declares a required permission.
- Do not copy code from Humanitec repositories.

## Review guidelines

- Check changes against the requirements and scenarios in `openspec/changes/initial-platform-orchestrator-spec/specs`. Flag behavior that contradicts a spec scenario, and tasks ticked in `tasks.md` whose verification is missing.
- Treat as high priority: secret values leaking into logs, errors, API responses or the planner; missing or wrong authorization checks; `404` versus `403` visibility mistakes; unsafe destruction or deprovisioning; non-idempotent retries of cloud mutations.
- Flag I/O, async or nondeterminism introduced into `crates/core`.
- Flag `unwrap`/`expect` on input that comes from users, drivers or the network.
- Flag dependencies whose licenses are not in the `deny.toml` allow-list.
