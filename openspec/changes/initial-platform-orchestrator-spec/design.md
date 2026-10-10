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
- Two vertical slices that really deploy, with the platform side defined only through the OpenTofu provider: `porch workload deploy` of a zip-packaged Python function with a DynamoDB table and an SQS queue trigger (no cluster), and of a container web workload with a `postgres` on RDS and an `s3` bucket into a namespace on EKS.

**Non-Goals:**

- Horizontal scaling of the server or multi-writer deployments. One server process owns all deployments in this milestone.
- Resuming a deployment across a server restart. An interrupted deployment is marked failed and can be re-run.
- Wire compatibility with Humanitec's API or `humctl`. Paths and shapes are similar by design, not identical.
- A web UI.

## Decisions

### D1. Rust workspace with a thin-core / fat-edge split

Cargo workspace:

- `crates/core`: domain types, placeholder parser and resolver, criteria matching, resource graph construction and ordering, set/delta algebra, workload document parsing and Score import. No async, no I/O, property-tested.
- `crates/store`: persistence behind a `Store` trait, implemented with `sqlx` on SQLite (default) with the schema written to also run on Postgres. Migrations embedded.
- `crates/drivers`: the `Driver` trait and built-in drivers (`echo`, `none`, `template`, `opentofu`, `k8s-cluster-static`, `k8s-cluster-eks`, external HTTP adapter).
- `crates/k8s`: the `openporch/kubernetes` workload driver: rendering to manifests and server-side apply via `kube`/`k8s-openapi`.
- `crates/engine`: the planner (graph walk, concurrency, retries, timeouts, logs, active-resource bookkeeping) on `tokio`, and the `Executor` trait with its in-process implementation (see D10).
- `crates/server`: `axum` HTTP API, auth, config, the `openporch` binary.
- `crates/client` and `crates/cli`: typed API client and the `porch` binary (`clap`).

Alternatives: a single crate (rejected: compile times and no enforced purity of the core); microservices (rejected: non-goal).

### D2. Storage: SQLite first, Postgres-ready schema

SQLite via `sqlx` keeps the single-binary story (one data directory, one file). The schema avoids SQLite-only features and is exercised against Postgres in CI from the start so the switch is a connection string. JSON columns hold sets, deltas and resolved inputs; sets are content-addressed by SHA-256 of their JCS-canonicalized JSON.

Alternatives: Postgres only (rejected: operational burden for a solo user); an embedded KV store (rejected: migrations and ad-hoc queries are harder).

### D3. Secrets: one reference model, stores behind it

Every secret the orchestrator knows about is a secret reference `{store, ref, version}`; there is no second code path for "inline" secrets. The built-in store `openporch` encrypts values with AES-256-GCM under a server master key (environment variable or file, key ID stored beside the ciphertext for rotation) and is the primary store until an administrator promotes an AWS Secrets Manager or SSM store. Inline `secrets` on definitions, cloud account keys and shared secret values are sugar: the API writes them to the primary store and keeps the reference. Because openporch has no in-cluster operator, the executor (D10) is the only reader: it resolves references immediately before a driver call using the store's cloud account, keeps values in memory for the call, and a redaction layer replaces every resolved value in logs and errors before persistence. Secret outputs of resources are written by the executor to the primary store and recorded as references; driver cookies are stored encrypted in the database with the same primitives.

Alternatives: Humanitec's split where the SaaS never reads secrets and an operator does (rejected: openporch is self-hosted, so the orchestrator already sits inside the trust boundary, and an operator is a non-goal); environment variables only (rejected: no rotation, no audit, no sharing).

### D4. The driver trait and execution model

```
trait Driver {
  async fn put(&self, req: DriverRequest) -> DriverResult<DriverResponse>;
  async fn get(&self, guresid, cookie) -> DriverResult<DriverStatus>;
  async fn delete(&self, guresid, req) -> DriverResult<DriverStatus>;
}
```

`DriverRequest` carries type, resolved resource inputs, resolved driver inputs (values and secrets), the cloud account credentials if any, context and cookie. Built-in drivers implement the trait in-process; the external HTTP adapter maps the trait onto Humanitec's Driver API 0.9.3 shapes so existing third-party drivers could be reused with minimal changes (header names differ: `Openporch-Driver-Cookie`). Asynchronous drivers return `InProgress`, and the engine polls `get` with the driver's hint, bounded by the call timeout.

Alternatives: drivers as separate processes or WASM plugins (rejected for this milestone: the external HTTP protocol already gives out-of-process extension).

### D5. OpenTofu driver runs `tofu` as a subprocess

Each `put`/`delete` gets a fresh working directory under the server's data dir, cloned with `git` (system binary) at the requested rev, with `files` and `script` written, and `tofu init`/`apply`/`destroy` run with a sanitized environment (only the credentials and `TF_*` variables openporch sets). Variables go through a generated `openporch.auto.tfvars.json` with secret variables in a second file that is never logged. Outputs are read with `tofu output -json`; `sensitive` outputs map to secret outputs.

State: if the code declares a backend it is used untouched. Otherwise the driver writes `openporch_backend_override.tf` declaring an `s3` backend at `s3://<bucket>/openporch/<guresid>/terraform.tfstate` (bucket, region and optional DynamoDB lock table from server config, credentials from the server's own AWS identity) or a `local` backend under the data dir when no bucket is configured (development only). This keeps locking and consistency inside OpenTofu.

Alternatives: pulling and pushing state files with `tofu state pull/push` (rejected: no locking, racy); in-process OpenTofu (not possible, it is a Go program). The `managed` and `custom-kubernetes` runner modes of Humanitec are collapsed to "runs where the server runs" for this milestone; a remote runner is a later change.

### D6. An openporch workload document instead of raw Score

Score v1b1 requires at least one container with an image and forbids unknown fields, and the Score project has no serverless roadmap (the closest thing is an open proposal for `x-` extension fields). A zip-packaged, event-driven function therefore has no honest representation in Score, and Kristian decided on 2026-10-10 that openporch must not depend on containers. openporch defines its own document, `openporch.dev/v1` `kind: Workload`, which keeps Score's container shape verbatim (`containers`, `service`, `resources`) so Score files import one to one, and adds what Score lacks: a `function` kind (package, runtime, handler, memory, timeout, concurrency), `triggers` (http, schedule, queue, topic, stream) that reference the workload's own `resources`, and `deploy`/`target` sections that replace Humanitec's separate extension file.

The overlap with Score is deliberate: `porch score deploy` remains an alias, score-compose still works for container workloads during local development, and if Score ever grows extension points the function block can be contributed upstream. Triggers point at dependencies rather than being resources themselves, so graph edges keep their direction (workload depends on queue) and the workload's own driver creates the mapping, schedule or URL inside the workload's state.

Artifacts: a zip is referenced the way an image is, by an immutable URI produced by CI (`s3://bucket/key` with a version ID or content hash), supplied with `--artifact`, and stored in the set so set IDs, diffs and rollback cover code changes. Uploading through the server for CI-less hobby use is a later addition.

Alternatives: Score plus a typed extension file (rejected by Kristian: still needs a placeholder container for functions); extensions inside the Score file via `metadata` (rejected: string-only annotations and target config leaking into the portable file); a separate function spec next to Score (rejected: two formats to version, convert and document).

### D7. Workload targets are ordinary `workload` definitions

The `workload` resource is matched by class like every other type, and the matched definition's driver decides the runtime. The orchestrator hands the driver the fully resolved spec as resource inputs. Three targets ship in this milestone: `openporch/none` (no runtime), AWS Lambda as a reference OpenTofu module under `examples/modules/aws-lambda-workload` used through the OpenTofu driver, and Kubernetes as the built-in `openporch/kubernetes` driver (rendering and server-side apply are too stateful and chatty to express well in a module). Kubernetes resources (`k8s-cluster`, `k8s-namespace`) enter the graph only through references in the Kubernetes workload definition, so a Lambda-only platform never sees them.

The Lambda target exists for cost: a hobby platform of functions, DynamoDB tables and queues is pay-per-use, while any Kubernetes cluster has a fixed monthly floor. The module keeps secrets out of Lambda environment variables (SSM SecureString parameters by default, which are free, with references injected) and builds the execution role from the `aws-policy` resources co-provisioned by dependency definitions, so least privilege falls out of the graph.

Alternatives: a built-in Rust Lambda driver (rejected: the module is auditable and editable by the platform team, and the AWS surface is large); ECS or App Runner as the first serverless target (rejected: both have fixed costs or are less common for hobby projects; they fit the same pattern later).

### D8. Kubernetes access

`kube` with `k8s-openapi` and server-side apply (field manager `openporch`). Cluster credentials come from the `k8s-cluster` resource's outputs: the static driver echoes server, CA and token or client certificate; the EKS driver describes the cluster with `aws-sdk-eks` using the cloud account's credentials and mints a token by presigning STS `GetCallerIdentity` the way `aws-iam-authenticator` does. Namespaces are created with the same client. No operator or agent is installed in the cluster.

Kubernetes is the last phase of the milestone. Until it lands, the `openporch/none` and Lambda workload targets make the whole AWS path (cloud accounts, OpenTofu driver, RDS, DynamoDB, S3, SQS, rollback, deprovisioning) deployable and testable without any cluster.

**Rendering is pure.** `crates/k8s` renders a resolved workload into a manifest list with no cluster access: no reads of live objects, no defaulting from the API server. Apply, readiness, removal and status are separate steps. This keeps the golden-file tests honest and leaves room for other ways to deliver manifests.

**Deferred: GitOps delivery.** Humanitec's Operator exists for customer-owned secret stores, private cluster endpoints and GitOps. The first two are answered by the executor boundary (D10). GitOps is not in milestone 1 (an independent review on 2026-10-10 concluded that a render/apply "sink" split now would change the driver contract and the direct-apply path for no milestone benefit). If it is added later:
(a) Select it per environment through the `k8s-cluster` resource, for example an `openporch/k8s-cluster-git` driver whose outputs name a repository, branch and path, matched by criteria like any cluster. No second workload driver.
(b) A deployment succeeds once the commit is pushed; readiness waiting does not apply. Runtime status needs a separate read-only cluster credential. The namespace and its removal become file changes in Git, not API calls.
(c) Secrets reach Git only as references, preferably as External Secrets Operator `ExternalSecret` objects pointing at the same AWS Secrets Manager or SSM store the orchestrator references. This requires an external primary store, because the built-in store would otherwise need an endpoint that returns values, which the secrets spec forbids. The executor and driver contracts gain a "references, not values" mode at that point, confined to the Kubernetes crate.
(d) Humanitec's custom resource definitions and in-cluster reconciliation are not adopted.

Alternatives: shelling out to `kubectl` (rejected: binary dependency and weaker typing); an in-cluster operator like Humanitec's (rejected: non-goal, adds a second component).

### D9. Placeholder engine

A hand-written parser turns strings into a list of literal and placeholder segments (`context.*`, `resources[...]...`, `externals.*`, `shared.*`, `values.*`, `pod.*`). The same AST feeds two passes: dependency discovery during graph construction (resource references add nodes and edges) and resolution during provisioning (typed substitution, secret tainting). Resolution errors carry the definition ID and JSON path of the offending input.

### D10. Deployment engine: a planner and an executor, one process by default

A deployment is a persisted job. The engine is split in two along the secret boundary:

- The **planner** builds the graph, persists it as the deployment's resource list, matches definitions, resolves every non-secret placeholder, and walks the graph with a `tokio` semaphore (default 4), starting any node whose dependencies are `provisioned`. Three attempts per node with a short backoff; a failed node leaves dependents `pending`. On completion it writes active resources and the environment's current set. A server restart marks `in_progress` deployments `failed` with reason `interrupted`.
- The **executor** takes one `ProvisionJob` at a time: the resolved non-secret inputs, the unresolved secret references, the cloud account to use and the driver to call. It resolves the references against the secret stores, obtains cloud credentials, runs the driver (in-process drivers, the OpenTofu subprocess, the Kubernetes client, or an external HTTP driver), writes any secret outputs to the primary secret store, and returns non-secret outputs, references for secret outputs, the driver cookie, logs and status.

The contract between them is the `Executor` trait, and milestone 1 ships exactly one implementation: `LocalExecutor`, a `tokio` task inside the server process. There is nothing extra to deploy, configure or monitor; `openporch serve` is still one binary and one data directory. The split costs one trait and one job type, and buys three things: the planner never holds a secret value, the server database never holds one either when the primary store is external, and a remote executor (an agent running in your AWS account or cluster that connects outbound and pulls jobs) is a later additive change with no spec impact.

Rules that keep the boundary honest: secret references cross the boundary unresolved; the executor's logs go through the redaction layer before leaving it; secret outputs never return as values; and the planner's tests use a fake executor, so the planner cannot accidentally depend on having the values.

Alternatives: a single engine that resolves secrets wherever convenient (rejected: cheap now, impossible to split later); shipping the remote agent in this milestone (rejected: adds a job queue, agent identity and log streaming to a milestone that should first work end to end).

### D11. OpenTofu provider written in Go

The OpenTofu plugin protocol has a supported framework only in Go (`terraform-plugin-framework`, which OpenTofu uses unchanged). The provider is a thin client of the REST API, so writing it in Go costs little, while implementing protocol 6 over gRPC in Rust would be a project of its own. The provider lives in `provider/` with its own Go module, generates docs with `tfplugindocs`, and is tested with the framework's acceptance tests against a local `openporch` server. Driver inputs are a `DynamicPseudoType` attribute so arbitrary JSON works without string-encoding.

Alternatives: Rust implementation of tfplugin6 (rejected: no framework, large surface, delays the milestone); a generic REST provider (rejected: no sensitivity handling, poor plans). Revisit when a Rust plugin framework matures.

### D12. Template driver language

The template driver uses `minijinja` (Jinja2 dialect) rather than Go templates: it is the mature Rust option and the spec only requires a documented template language. Humanitec template-driver examples therefore need a syntax translation; the driver docs carry a side-by-side table.

### D13. Identity: principals, hashed tokens, signed sessions, pluggable providers

Authentication resolves every request to a principal before any handler runs. API tokens are random 256-bit values shown once and stored as SHA-256 hashes with expiry and revocation. Sessions are compact signed tokens (EdDSA, 24-hour expiry, `sub` = principal ID, `jti` for revocation) issued after a login through an identity provider. Providers are an enum behind one trait: `github` (OAuth app, device authorization flow for the CLI, authorization code for browsers; the GitHub login is the external identity) and generic `oidc` (authorization code with PKCE; `issuer:sub` is the identity). GitHub is a configured provider, not special-cased, so adding Google or an enterprise IdP is one more variant. The first administrator is a `bootstrap` service user whose token the server prints once on first start, which avoids any password or email dependency.

Alternatives: passwords (rejected: another credential class to protect and no one wants them); SAML (deferred: the pieces exist but hobby users do not have an IdP).

### D14. Authorization: declared per route, enforced centrally

Each axum route is registered with a required permission (an enum such as `App(Read)`, `Deploy(EnvType)`, `Org(ManageResources)`), and an extractor evaluates it against the principal's effective roles (organization role, application roles, environment type roles, administrator implying all) before the handler sees the request. A startup check fails the process if any route lacks a permission, and a test walks the router to assert the same, so authorization cannot be forgotten on a new endpoint. Role IDs and semantics follow Humanitec so documentation and mental models carry over; the permission matrix lives in the `rbac` spec and in one table in code. Visibility rules (`404` for applications the principal may not list) are part of the same extractor. The audit log is a middleware that records successful mutating requests with the principal after the handler returns.

Alternatives: policy engines like Casbin or OPA (rejected for now: the role model is small and fixed, and an in-process table is simpler to test and ship; a policy engine can replace the evaluator later behind the same extractor).

### D15. OIDC provider, sequenced last

Issuing identity tokens so AWS can trust openporch without standing credentials is the right end state, but it needs a public HTTPS URL and key management, neither of which a first end-to-end run needs. The design reserves the pieces now: the cloud account type enum includes `aws-identity`, the credential resolver is a trait with one implementation per account type, and signing keys live in the same encrypted key table as the session key. The issuer uses RS256 (AWS requires it), publishes a JWKS with two keys during rotation, mints tokens with `sub = <org>/<account-id>`, `aud = sts.amazonaws.com` and a 15-minute lifetime, and exchanges them with `AssumeRoleWithWebIdentity`. Until it ships, `aws-role` with an external ID gives role assumption from whatever identity the orchestrator host already has (an instance profile, or a developer's own credentials).

### D16. API shape mirrors Humanitec paths

Paths are `/orgs/{org}/apps/{app}/envs/{env}/...`, `/orgs/{org}/resources/{defs,types,classes,accounts,drivers}`, `/orgs/{org}/env-types`. IDs follow Humanitec's rules. This keeps `humctl` users and the docs' examples recognizable and leaves room for multiple orgs later without a path change.

## Risks / Trade-offs

- [Scope is large for a first milestone] → tasks are grouped into phases that each leave a usable subset (pure core and API first, then echo/template and OpenTofu drivers, the engine, the CLI, the provider, an AWS-only reference platform, and Kubernetes last so the AWS resource path is proven first); each phase has its own tests and can ship alone.
- [Lambda's constraints (ECR-only images, 15 minute max timeout, no long-lived ports) do not fit every container workload, and Kubernetes cannot run function workloads or queue triggers] → each driver fails fast with clear errors for unsupported shapes; an HTTP container workload switches between Lambda and Kubernetes by changing `metadata.class`.
- [Owning a workload format means owning its evolution] → the document is versioned by `apiVersion`, keeps Score's container shape byte-compatible, and gets a JSON Schema published with the CLI.
- [Running `tofu` inside the server exposes the server's filesystem and network to module code] → runs use a dedicated working dir per GUResID, a sanitized environment, and a documented recommendation to run the server as an unprivileged user; a remote runner is the follow-up.
- [EKS token minting and AWS SDK details are easy to get subtly wrong] → an opt-in e2e test against a real EKS cluster, plus unit tests of the presigned URL against known-good vectors.
- [Secret leakage through logs and errors] → single redaction layer applied before persistence, tested with secrets embedded in OpenTofu output and error text.
- [Two toolchains (Rust and Go)] → the Go code is confined to `provider/`, built and tested in its own CI job.
- [Content-addressed sets diverge if canonicalization changes] → JCS canonicalization is pinned and tested with fixtures; the set ID algorithm is versioned in the stored record.
- [RBAC and secrets touch every endpoint, so retrofitting is impossible] → they are implemented in group 3 before any domain endpoint, every route declares its permission, and the HTTP tests for each capability include a forbidden-role case.
- [The orchestrator reads secrets itself] → only the executor does, behind the `Executor` trait, so a remote executor can move that work into your account later; meanwhile the mitigation is minimal exposure (resolve late, hold briefly, redact always), secret outputs stored as references, and an audit log of every change.
- [SQLite's single-writer model under concurrent deployments] → one engine process with WAL mode and short transactions; Postgres is the escape hatch.

## Migration Plan

Greenfield: there is nothing to migrate. Deployment is one binary plus a data directory (`openporch serve --data-dir ...`), with database migrations applied on start. Rollback of the server is reinstalling the previous binary; migrations in this milestone are additive only.

## Open Questions

- GitOps delivery and how its secrets are handled (see D8).
- Which Kubernetes minor versions to support officially (affects only the `k8s-openapi` feature flag and the test matrix).
- Whether the first release ships a container image in addition to the binary.
- When to promote Postgres from "CI-tested" to "documented for production".
