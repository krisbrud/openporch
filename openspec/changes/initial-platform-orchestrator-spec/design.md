# Design

## Context

The repository is empty apart from a README. See proposal.md for why the project exists and what milestone 1 covers. The constraints that shape the design:

- One engineer self-hosts this. A single binary with an embedded database must be a first-class deployment, and nothing may require a Kubernetes operator, agent or SaaS component to work.
- Platform configuration is OpenTofu; developer input is the openporch workload document, with Score accepted as an import. Humanitec's vocabulary and object model are kept so its docs, examples and mental model carry over, but no code is copied from Humanitec repositories (their examples are Apache-2.0; openporch is Apache-2.0 too, and any inspiration is re-implemented).
- AWS is the first cloud: credentials, OpenTofu state and the managed-cluster driver all target AWS. Nothing in the core may assume AWS, so other clouds can follow as drivers.
- The orchestrator must run `tofu` itself, so it needs a filesystem, network egress to Git and AWS, and the `tofu` binary on its PATH.

## Goals / Non-Goals

**Goals:**

- A control plane whose public surfaces (REST paths, CLI verbs, provider resources, driver HTTP protocol, workload document) are stable enough that later milestones add to them without renaming.
- A pure, well-tested core (placeholders, matching, graph, delta application) that has no I/O, so most of the orchestrator's logic is verifiable without a cluster or an AWS account.
- Staged vertical slices, with the first release focused on Lambda and Kubernetes following in milestone 2, both configured through the OpenTofu provider: `porch workload deploy` of a zip-packaged Python function with a DynamoDB table and an SQS queue trigger (no cluster), and of a container web workload with a `postgres` on RDS and an `s3` bucket into a namespace on EKS.

**Non-Goals:**

- Horizontal scaling of the server or multi-writer deployments. One server process owns all deployments in this milestone.
- Automatically resuming an entire deployment across a server restart. Interrupted deployments fail; durable per-resource recovery is mandatory before safe re-execution.
- Wire compatibility with Humanitec's API or `humctl`. Paths and shapes are similar by design, not identical.
- A web UI.

## Decisions

### D1. Rust workspace with a thin-core / fat-edge split

Cargo workspace:

- `crates/core`: domain types, placeholder parser and resolver, criteria matching, resource graph construction and ordering, set/delta algebra, workload document parsing and Score import. No async, no I/O, property-tested.
- `crates/store`: persistence behind a `Store` trait, implemented with `sqlx` on SQLite first; embedded versioned migrations and the trait boundary leave room for a later Postgres implementation.
- `crates/drivers`: the `Driver` trait and built-in drivers (`echo`, `none`, `template`, `opentofu`, `k8s-cluster-static`, `k8s-cluster-eks`, external HTTP adapter).
- `crates/k8s`: the `openporch/kubernetes` workload driver: rendering to manifests and server-side apply via `kube`/`k8s-openapi`.
- `crates/engine`: the planner (graph walk, concurrency, retries, timeouts, logs, active-resource bookkeeping) on `tokio`, and the `Executor` trait with its in-process implementation (see D10).
- `crates/server`: `axum` HTTP API, auth, config, the `openporch` binary.
- `crates/client` and `crates/cli`: typed API client and the `porch` binary (`clap`).

Alternatives: a single crate (rejected: compile times and no enforced purity of the core); microservices (rejected: non-goal).

### D2. Storage: SQLite first, one writer

SQLite via `sqlx` keeps the first release to one data directory. A `Store` boundary and versioned migrations leave room for Postgres later; maintaining a second backend and its CI matrix is not a milestone 1 requirement. Adding Postgres requires documented data migration and the same recovery/concurrency tests, not merely a new connection string. Both backends remain single-engine until explicitly designed otherwise.

The server acquires exclusive installation ownership before accepting mutations. Short transactions persist operation intent and audit intent together; per-resource results are checkpointed before dependents run. Sets use SHA-256 over pinned JCS canonicalization. The environment separately records desired and last-successful sets, generation and partial/unknown convergence.

Alternatives: Postgres only (rejected: operational burden); an embedded KV store (rejected: migration/query complexity).

### D3. Secrets: references, use grants and protected artifacts

The built-in `openporch` store encrypts values with AES-256-GCM under a configured master key, with key IDs and tested rotation/restore behavior. It is the first-release primary store; AWS Secrets Manager and SSM store integrations follow in milestone 2. Inline values are write-only convenience inputs converted to references. Reference possession does not grant use: application/environment grants are checked at assignment and by the executor, including replay. Developers who can deploy a workload can access secrets authorized to that workload; hiding API reads is not isolation from workload authors.

The planner carries references and tainted expression trees. Only the executor resolves values and renders composite secret strings; it stores secret outputs as versioned references and encrypts cookies. Logs are redacted before leaving execution. OpenTofu needs secret variables files and can persist plaintext secrets inside its state representation, so this is not a memory-only guarantee: temporary files are owner-only and cleaned after safe recovery; state, plans, keys and backups follow the protected-artifact policy. The `sensitive` flag is not encryption.

The local executor and platform modules are trusted within the host boundary. A future remote executor needs explicit identity, transport, leasing and recovery protocol work; the trait boundary helps but does not make that change spec-free.

### D4. The driver trait and execution model

```
trait Driver {
  async fn plan(&self, req: DriverRequest) -> DriverResult<DriverPlan>; // unknown if unsupported
  async fn put(&self, req: DriverRequest) -> DriverResult<DriverResponse>;
  async fn get(&self, guresid, cookie) -> DriverResult<DriverStatus>;
  async fn delete(&self, guresid, req) -> DriverResult<DriverStatus>;
}
```

`DriverRequest` carries durable operation ID, input fingerprint, type, resolved resource/driver inputs, ephemeral cloud credentials, context and versioned cookie. Registrations publish protocol version, schemas and recovery/preview/target capabilities. Identical operation retries must converge to one effect; unsupported recovery blocks blind retries after uncertainty. Built-in drivers implement the trait in-process; the external HTTP adapter maps the trait onto an explicitly documented profile of Humanitec Driver API 0.9.3 shapes, with openporch cookie and operation-identity headers. Compatibility fixtures document differences; legacy drivers do not acquire idempotency guarantees merely through the adapter. Asynchronous drivers return `InProgress`, and the engine polls `get` with the driver's hint, bounded by the call timeout.

Alternatives: drivers as separate processes or WASM plugins (rejected for this milestone: the external HTTP protocol already gives out-of-process extension).

### D5. OpenTofu runs with retained identity and saved plans

Each operation gets a restricted working directory, source pinned to a resolved commit, retained dependency lock information and a stable GUResID-to-backend mapping. Generated files reject traversal/symlink escape; secret inputs go into owner-only variables files and never command-line arguments. The driver runs noninteractive init and saved-plan creation, inspects machine-readable actions, verifies safety permits and applies that exact plan. Changed inputs or stale state require re-planning. Destroy uses retained source/state, its own authorized plan and confirmed completion before state cleanup.

An administrator-approved non-local module backend is used only with state separation, locking and protected-state controls. A module's local backend is overridden with the orchestrator-managed backend: S3 when configured, otherwise persistent local state for a single-host installation. Local encrypted storage is supported for hobby use; S3 is optional. A definition edit cannot silently change an active resource's backend or driver. State is never an ephemeral working-directory artifact.

The subprocess receives only its required credentials/configuration. It remains trusted code, not a sandbox: modules can execute code with the server account's privileges. Timeout terminates and reaps the process group, then recovery inspects state/observations before retry. Package checksums, lockfiles and TLS verification remain enabled. Remote execution is deferred.

### D6. An openporch workload document instead of raw Score

Score v1b1 requires at least one container with an image and forbids unknown fields, and the Score project has no serverless roadmap (the closest thing is an open proposal for `x-` extension fields). A zip-packaged, event-driven function therefore has no honest representation in Score, and Kristian decided on 2026-10-10 that openporch must not depend on containers. openporch defines its own document, `openporch.dev/v1` `kind: Workload`, which keeps Score's container shape verbatim (`containers`, `service`, `resources`) so Score files import one to one, and adds what Score lacks: a `function` kind (package, runtime, handler, memory, timeout, concurrency), `triggers` (http, schedule, queue, topic, stream) that reference the workload's own `resources`, and `deploy`/`target` sections that replace Humanitec's separate extension file.

The overlap with Score is deliberate: `porch score deploy` remains an alias, score-compose still works for container workloads during local development, and if Score ever grows extension points the function block can be contributed upstream. Triggers point at dependencies rather than being resources themselves, so graph edges keep their direction (workload depends on queue) and the workload's own driver creates the mapping, schedule or URL inside the workload's state.

Artifacts: CI supplies a zip URI through `--artifact`; acceptance resolves an S3 object version or verifies a content-addressed object under an enforced immutability policy. A hash-looking key alone is insufficient. Image tags similarly resolve to digests. Snapshots retain these identities for replay even when developer input uses a convenient tag. Uploading through the server for CI-less hobby use is a later addition.

Alternatives: Score plus a typed extension file (rejected by Kristian: still needs a placeholder container for functions); extensions inside the Score file via `metadata` (rejected: string-only annotations and target config leaking into the portable file); a separate function spec next to Score (rejected: two formats to version, convert and document).

### D7. Workload targets are ordinary `workload` definitions

The `workload` resource is matched by class like every other type, and the matched definition's driver decides the runtime. The orchestrator hands the driver the fully resolved spec as resource inputs. Targets are staged: milestone 1 ships `openporch/none` (no runtime), AWS Lambda as a reference OpenTofu module under `examples/modules/aws-lambda-workload` used through the OpenTofu driver; milestone 2 adds Kubernetes as the built-in `openporch/kubernetes` driver (rendering and server-side apply are too stateful and chatty to express well in a module). Kubernetes resources (`k8s-cluster`, `k8s-namespace`) enter the graph only through references in the Kubernetes workload definition, so a Lambda-only platform never sees them.

The Lambda target offers a pay-per-use alternative to a continuously running cluster. Control-plane hosting, cloud usage and retained state/artifacts still have costs. The module keeps secrets out of Lambda environment variables (SSM SecureString by default, with references injected) and builds the execution role from co-provisioned `aws-policy` resources with tested action/resource-scoped policies. Graph edges ensure policies finish before consumers; graph structure alone is not a least-privilege proof.

Alternatives: a built-in Rust Lambda driver (rejected: the module is auditable and editable by the platform team, and the AWS surface is large); ECS or App Runner as the first serverless target (rejected: both have fixed costs or are less common for hobby projects; they fit the same pattern later).

### D8. Kubernetes access

`kube` with `k8s-openapi` and server-side apply (field manager `openporch`). Cluster credentials come from the `k8s-cluster` resource's outputs: the static driver echoes server, CA and token or client certificate; the EKS driver describes the cluster with `aws-sdk-eks` using the cloud account's credentials and mints a token by presigning STS `GetCallerIdentity` the way `aws-iam-authenticator` does. Namespaces are created with the same client. No operator or agent is installed in the cluster.

Kubernetes is milestone 2 and is not a first-release gate. Until it lands, the `openporch/none` and Lambda workload targets make the whole AWS path (cloud accounts, OpenTofu driver, RDS, DynamoDB, S3, SQS, rollback, deprovisioning) deployable and testable without any cluster.

**Rendering is pure.** `crates/k8s` renders a resolved workload into a manifest list with no cluster access: no reads of live objects, no defaulting from the API server. Apply, readiness, removal and status are separate steps. This keeps the golden-file tests honest and leaves room for other ways to deliver manifests.

**Deferred: GitOps delivery.** Humanitec's Operator exists for customer-owned secret stores, private cluster endpoints and GitOps. The executor boundary (D10) permits a later remote-execution design for the first two; a local executor still needs actual network access to private endpoints. GitOps is outside the first two milestones (an independent review on 2026-10-10 concluded that a render/apply "sink" split now would change the driver contract and the direct-apply path for no milestone benefit). If it is added later:
(a) Select it per environment through the `k8s-cluster` resource, for example an `openporch/k8s-cluster-git` driver whose outputs name a repository, branch and path, matched by criteria like any cluster. No second workload driver.
(b) A deployment succeeds once the commit is pushed; readiness waiting does not apply. Runtime status needs a separate read-only cluster credential. The namespace and its removal become file changes in Git, not API calls.
(c) Secrets reach Git only as references, preferably as External Secrets Operator `ExternalSecret` objects pointing at the same AWS Secrets Manager or SSM store the orchestrator references. This requires an external primary store, because the built-in store would otherwise need an endpoint that returns values, which the secrets spec forbids. The executor and driver contracts gain a "references, not values" mode at that point, confined to the Kubernetes crate.
(d) Humanitec's custom resource definitions and in-cluster reconciliation are not adopted.

Alternatives: shelling out to `kubectl` (rejected: binary dependency and weaker typing); an in-cluster operator like Humanitec's (rejected: non-goal, adds a second component).

### D9. Placeholder engine

A hand-written parser turns strings into a list of literal and placeholder segments (`context.*`, `resources[...]...`, `externals.*`, `shared.*`, `values.*`, `pod.*`). The same AST feeds two passes, preserving unresolved tainted expressions across the planner/executor boundary: dependency discovery during graph construction (resource references add nodes and edges) and resolution during provisioning (typed substitution, secret tainting). Resolution errors carry the definition ID and JSON path of the offending input.

### D10. Durable planner and executor, one process by default

The planner accepts deployments with an expected environment generation and idempotency key. Acceptance atomically persists desired state, job and audit intent; it never overwrites last-successful state on acceptance. Preflight validates the complete graph, scoped references, target capabilities and policy before resource mutation. A stored execution snapshot captures definition content, versions, immutable artifacts, values and secret references, while active-resource records retain the information needed for recovery and destruction.

Before a driver mutation, the engine persists GUResID, operation identity, input fingerprint and recovery metadata. Each result/cookie is checkpointed before dependent work runs. A global semaphore defaults to four driver operations across all environments. Permanent errors do not retry; transient errors use bounded jittered backoff within the deployment deadline. Timeout is an uncertain outcome until cancellation/observation establishes otherwise. Restart marks interrupted deployments failed, then resolves uncertain resources before conflicting mutations are admitted. It need not automatically finish the whole deployment.

The executor resolves authorized secrets late, obtains fresh credentials, plans where supported, enforces the server's action permits, invokes drivers and returns only non-secret outputs, secret references, redacted logs and recovery metadata. Milestone 1 has one `LocalExecutor` in the server process. Tests use a fake executor for graph logic and crash/timeout fault injection for the durable boundary.

Snapshot replay uses recorded configuration; explicit current-configuration redeploy rematches the old workload set. Neither restores application data nor bypasses current grants/policy. Retained non-workload dependencies survive ordinary removal; removed workloads lose their owned runtimes/triggers on every target. Destruction, replacement and unknown-impact changes use resource-specific, expiring permits bound to preview fingerprints; protected environments require administrator approval and known effects. A separate approval service is unnecessary.

### D11. OpenTofu provider written in Go

The OpenTofu plugin protocol has a supported framework only in Go (`terraform-plugin-framework`, which OpenTofu uses unchanged). The provider is a thin client of the REST API, so writing it in Go costs little, while implementing protocol 6 over gRPC in Rust would be a project of its own. The provider lives in `provider/` with its own Go module, generates docs with `tfplugindocs`, and is tested with the framework's acceptance tests against a local `openporch` server. Driver inputs are a `DynamicPseudoType` attribute so arbitrary JSON works without string-encoding.

Alternatives: Rust implementation of tfplugin6 (rejected: no framework, large surface, delays the milestone); a generic REST provider (rejected: no sensitivity handling, poor plans). Revisit when a Rust plugin framework matures.

### D12. Template driver language

The template driver uses `minijinja` (Jinja2 dialect) rather than Go templates: it is the mature Rust option and the spec only requires a documented template language. Humanitec template-driver examples therefore need a syntax translation; the driver docs carry a side-by-side table.

### D13. Identity: principals, hashed tokens, signed sessions, pluggable providers

Authentication resolves every request to a principal before any handler runs. API tokens are random 256-bit values shown once and stored as SHA-256 hashes with expiry and revocation. Sessions are compact signed tokens (EdDSA, 24-hour expiry, `sub` = principal ID, `jti` for revocation) issued after a login through an identity provider. Providers are an enum behind one trait: `github` (OAuth app, device authorization flow for the CLI, authorization code for browsers; the immutable GitHub numeric user ID is the external identity) and generic `oidc` (authorization code with PKCE; `issuer:sub` is the identity). GitHub is a configured provider, not special-cased, so adding Google or an enterprise IdP is one more variant. Every request checks current membership, role and revocation state even when its session signature is valid. Only administrators may delegate administrator privileges; manager-issued service tokens cannot exceed the issuer's authority. The first administrator is a `bootstrap` service user whose short-lived token is written once to an owner-only file, never normal logs. GitHub login is the first-release external provider; generic OIDC login follows in milestone 2.

Alternatives: passwords (rejected: another credential class to protect and no one wants them); SAML (deferred: the pieces exist but hobby users do not have an IdP).

### D14. Authorization: declared per route, enforced centrally

Each axum route is registered with a required permission (an enum such as `App(Read)`, `Deploy(EnvType)`, `Org(ManageResources)`), and an extractor evaluates it against the principal's effective roles (organization role, application roles, environment type roles, administrator implying all) before the handler sees the request. A startup check fails the process if any route lacks a permission, and a test walks the router to assert the same, so authorization cannot be forgotten on a new endpoint. Role IDs and semantics follow Humanitec so documentation and mental models carry over; the permission matrix lives in the `rbac` spec and in one table in code. Visibility rules (`404` for applications the principal may not list) are part of the same extractor. Route checks are supplemented by reference-scope and delegation checks. Audit intent is committed atomically with each accepted mutation, with correlated execution outcomes and denied-privilege events. Thirty-day default retention and export suit small installations; teams can configure longer retention.

Alternatives: policy engines like Casbin or OPA (rejected for now: the role model is small and fixed, and an in-process table is simpler to test and ship; a policy engine can replace the evaluator later behind the same extractor).

### D15. OIDC provider, later integration

Issuing identity tokens so AWS can trust openporch without standing credentials is a later optional integration, but it needs a public HTTPS URL and key management, neither of which a first end-to-end run needs. The design reserves the pieces now: the cloud account type enum includes `aws-identity`, the credential resolver is a trait with one implementation per account type, and signing keys live in the same encrypted key table as the session key. The issuer uses RS256 (AWS requires it), publishes a JWKS with two keys during rotation, mints tokens with `sub = <org>/<account-id>`, `aud = sts.amazonaws.com` and a 15-minute lifetime, and exchanges them with `AssumeRoleWithWebIdentity`. Until it ships, `aws-role` with an external ID gives role assumption from whatever identity the orchestrator host already has (an instance profile, or a developer's own credentials).

### D16. API shape mirrors Humanitec paths

Paths are `/orgs/{org}/apps/{app}/envs/{env}/...`, `/orgs/{org}/resources/{defs,types,classes,accounts,drivers}`, `/orgs/{org}/env-types`. IDs follow Humanitec's rules. This keeps `humctl` users and the docs' examples recognizable and leaves room for multiple orgs later without a path change.

## Delivery and operational contracts

Milestone 1 is a safe SQLite/CLI/API/OpenTofu/Lambda vertical slice. Milestone 2 adds Kubernetes, AWS secret-store adapters, generic OIDC login, broader examples and provider integrations. Postgres, the cloud OIDC issuer, remote execution, drift automation and HA are later work. `proposal.md` and `tasks.md` define release gates; retained later-stage specs are design contracts, not first-release dependencies.

`deployment-safety` owns ownership, previews, permits and the trusted-code boundary. `control-plane-operations` owns readiness, exclusive ownership, backup/restore, upgrade compatibility, uninstall, bounded work, diagnostics and release policy. Readiness, exclusive ownership, bounded work, destructive-action permits and ownership records are milestone 1 requirements. Backup/restore drills, upgrade fixtures, the capacity benchmark, release provenance and protected environment types are first-release hardening that follows milestone 1 (tasks group 14). Single-organization authorization is supported; hostile-tenant or plugin isolation is not claimed. Kubernetes rendering remains pure, and GitOps stays deferred as recorded in D8.

## Risks / Trade-offs

- [Scope is large] → first ship the safe Lambda slice; stage Kubernetes and additional integrations later. Internal phases have tests, but a release requires the complete vertical slice and operational/recovery gates.
- [Lambda's constraints (ECR-only images, 15 minute max timeout, no long-lived ports) do not fit every container workload, and Kubernetes cannot run function workloads or queue triggers] → each driver fails fast with clear errors for unsupported shapes; an HTTP container workload switches between Lambda and Kubernetes by changing `metadata.class`.
- [Owning a workload format means owning its evolution] → the document is versioned by `apiVersion`, keeps Score's container shape byte-compatible, and gets a JSON Schema published with the CLI.
- [Trusted modules share the server host boundary] → require an unprivileged server and input/policy controls, state explicitly that directories/environment filtering are not sandboxing, and defer hostile-plugin isolation to a separately designed executor.
- [EKS token minting and AWS SDK details are easy to get subtly wrong] → an opt-in e2e test against a real EKS cluster, plus unit tests of the presigned URL against known-good vectors.
- [Secret leakage through logs and errors] → single redaction layer applied before persistence, tested with secrets embedded in OpenTofu output and error text.
- [Two toolchains (Rust and Go)] → the Go code is confined to `provider/`, built and tested in its own CI job.
- [Content-addressed sets diverge if canonicalization changes] → JCS canonicalization is pinned and tested with fixtures; the set ID algorithm is versioned in the stored record.
- [RBAC and secrets touch every endpoint, so retrofitting is impossible] → they are implemented in group 3 before any domain endpoint, every route declares its permission, and the HTTP tests for each capability include a forbidden-role case.
- [The orchestrator reads secrets itself] → only the executor does, behind the `Executor` trait, so a remote executor can move that work into your account later; meanwhile the mitigation is minimal exposure (resolve late, hold briefly, redact always), secret outputs stored as references, and an audit log of every change.
- [SQLite under concurrent deployments] → one fenced engine, WAL mode, short transactions and bounded global work; benchmark the supported envelope before adding another database.

## Migration Plan

The initial schema is greenfield, but the first release must establish versioned migrations and protected backup/restore. Installation is one binary plus data/state/key material. Startup rejects unsupported schema versions before mutation. Each release tests supported upgrade paths; downgrade is permitted only for tested binary/schema pairs, otherwise restore a compatible backup and reconcile external state in recovery mode. Additive migrations do not alone prove downgrade safety. Uninstall retains infrastructure unless explicit permitted teardown is requested.

## Open Questions

- GitOps delivery and how its secrets are handled (see D8).
- Which Kubernetes minor versions to support officially (affects only the `k8s-openapi` feature flag and the test matrix).
- Whether the first release ships a container image in addition to the binary.
- When measured SQLite limits justify adding Postgres, its data migration path and a second CI matrix.
