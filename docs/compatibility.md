# Compatibility

openporch is pre-1.0. Each release lists here the contracts it serves and
reads, the formats it persists, and the combinations its CI tests. A contract
whose version changes breaks clients written for the old one; before 1.0 such
a change ships with a migration guide in the release notes instead of a new
major version.

## Contracts

| Contract | Version this release supports | Where it is declared | When it is unsupported |
| --- | --- | --- | --- |
| HTTP API | `1` | The `Openporch-Contract` header on every response, and `contract` in `GET /version`; [API reference](api.md) | `porch` and the provider send the contract they were written for, which a server of another contract refuses before doing anything (`400` `API-008`), and refuse a server whose responses name another contract. A request a route does not know gets `404`; an unknown field in a body gets `400` |
| Workload documents | `apiVersion: openporch.dev/v1`, `kind: Workload` | [`schemas/workload-v1.json`](../schemas/workload-v1.json) | Refused with `400` naming the field |
| Score import | `apiVersion: score.dev/v1b1` | `crates/core/src/workload.rs` | Refused with `400` |
| Resource type catalog | `v1` | [`schemas/catalog/v1`](../schemas/catalog/v1) | Inputs and outputs outside a type's schema are refused |
| HTTP drivers | `openporch-http/1` | `Openporch-Protocol` header; [HTTP drivers](drivers/http.md) | Registering or calling a driver that reports another version fails, naming both versions |
| Built-in drivers | `builtin` | `openporch/echo`, `openporch/none`, `openporch/template`, `openporch/opentofu` | Not applicable: they ship with the server |
| Driver cookies | Each driver's `cookie_version` | `DriverInfo` | A cookie of another version is not kept, and the operation's outcome is recorded as unknown until it is observed |
| OpenTofu provider | Plugin protocol 6, against API contract `1` | `provider/main.go` | OpenTofu reports the protocol mismatch |

## Persisted formats

| Format | Version | Compatibility |
| --- | --- | --- |
| SQLite database | Schema 11 (`openporch version` prints it) | Migrated forward on start from every earlier schema; a newer schema is refused before anything is written. [Upgrades](operations.md#upgrades-and-the-schema-version) lists the tested pairs |
| Schema fixtures | 8, 9, 10, 11 in [`crates/server/fixtures/schema`](../crates/server/fixtures/schema) | Every build upgrades each fixture and serves what it recorded |
| Backups | Format `1` (`backup.json`) | `openporch restore` refuses another format, a newer schema, another organization or another state backend |
| Sealed secrets and cookies | Master key IDs `mk-…` | A server missing a key a record names is not ready and names the key ID |
| OpenTofu state | Whatever the configured `tofu` writes | openporch reads only outputs; state is upgraded by OpenTofu |

## Owned schemas

openporch publishes and versions these schemas; tests fail when the code and
the published file differ.

- [`schemas/workload-v1.json`](../schemas/workload-v1.json): workload documents.
- [`schemas/catalog/v1`](../schemas/catalog/v1): one file per built-in
  resource type, with its inputs, outputs and secret outputs.
- The HTTP driver protocol in [drivers/http.md](drivers/http.md).
- The OpenTofu provider's schema, documented in [provider/docs](../provider/docs).

## Tested matrix

CI tests exactly these combinations on every pull request. Anything else is
untested, not necessarily broken.

| Component | Tested with |
| --- | --- |
| Server and `porch` | Rust 1.97.0 (`rust-toolchain.toml`), Linux x86_64 (`ubuntu-latest`) |
| OpenTofu driver and example modules | OpenTofu 1.11.5, Floci 2.2.0 as the AWS endpoint |
| OpenTofu provider | Go 1.25.8 (`provider/go.mod`), OpenTofu 1.11.5, the server built from the same commit |
| AWS account and artifact checks | Floci 2.2.0 |
| Real AWS | `just e2e-aws` against a sandbox account, run by hand ([e2e](e2e.md)) |
| Benchmark | `just bench` on two CPUs, run by hand ([benchmark](benchmark.md)) |

The release workflow also builds Linux aarch64 and macOS arm64 binaries. They
are built from the same locked inputs, but only Linux x86_64 runs the tests.

The provider and the server are released together. A provider talks to a
server of the same release; a newer provider can send attributes an older
server does not know, which the server refuses with `400`. For example,
`openporch_environment_type.protected` needs a server with schema 9 or later.
Likewise, `openporch_driver` needs a server with
`PUT /orgs/{org}/resources/drivers/{driver}`, which an older server does not
have, so its changes are refused there; registering and deleting drivers work.

Integrations planned for later milestones are not available in this release:
Kubernetes and EKS targets, AWS Secrets Manager and SSM secret stores, generic
OIDC login, Postgres, remote executors and GitOps delivery.
