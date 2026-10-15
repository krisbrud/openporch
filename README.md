# openporch

openporch is an open source platform orchestrator written in Rust. It follows the object model of the [Humanitec Platform Orchestrator](https://developer.humanitec.com/platform-orchestrator/docs/introduction/overview/): developers describe a workload and the resources it needs, platform engineers configure resource definitions and matching criteria, and the orchestrator resolves a resource graph and provisions it through drivers.

Platforms are configured with [OpenTofu](https://opentofu.org/) through the openporch provider. AWS is the first cloud, and serverless workloads (Lambda, DynamoDB, SQS, S3) deploy without Kubernetes. Kubernetes support follows in a later milestone.

openporch runs as a single binary with an embedded SQLite database and an in-process executor, so one person can self-host it.

## Status

openporch is under active development. The milestone 1 specification lives in [`openspec/changes/initial-platform-orchestrator-spec`](openspec/changes/initial-platform-orchestrator-spec), and [`tasks.md`](openspec/changes/initial-platform-orchestrator-spec/tasks.md) tracks implementation progress.

## Layout

| Crate | Purpose |
| --- | --- |
| `crates/core` | Pure domain logic: types, placeholders, criteria matching, resource graphs, deployment sets, workload documents |
| `crates/store` | Persistence behind the `Store` trait, SQLite first |
| `crates/drivers` | The `Driver` trait and built-in drivers, including OpenTofu |
| `crates/engine` | The deployment planner and the in-process executor |
| `crates/server` | The HTTP API and the `openporch` binary |
| `crates/client` | Typed API client |
| `crates/cli` | The `porch` command-line interface |
| `provider` | The OpenTofu provider, a Go module |

## Getting started

The [developer quickstart](docs/quickstart.md) runs a server locally and deploys
a workload with the `porch` command line. The [command reference](docs/cli.md)
lists every `porch` command, and the [API reference](docs/api.md) every route.

## Development

You need the Rust toolchain pinned in `rust-toolchain.toml` (rustup installs it automatically) and optionally [`just`](https://github.com/casey/just) and [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny).

```sh
just test   # fmt, clippy and unit tests, as CI runs them
just e2e    # end-to-end tests
just deny   # dependency license and advisory checks
just dev    # run the server locally
just docs   # run the documented examples and the quickstart against a dev server
just provider  # test the OpenTofu provider and load it into OpenTofu (needs Go and tofu)
just example-platform  # apply examples/aws-platform against a dev server and AWS or Floci, then destroy it
```

## License

openporch is licensed under the [Apache License 2.0](LICENSE). It does not copy code from Humanitec repositories; any inspiration from their Apache-2.0 examples is reimplemented.
