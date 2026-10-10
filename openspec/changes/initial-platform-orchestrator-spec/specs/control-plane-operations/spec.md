# Spec Delta

## Purpose

Make a small single-server installation operable, recoverable and bounded without requiring high availability, a separate monitoring stack or a Kubernetes control plane.

## ADDED Requirements

### Requirement: First-release installation profile

Milestone 1 SHALL document an unprivileged SQLite server, persistent data/key directory and supported OpenTofu binary. Ship local none/echo and AWS Lambda quickstarts listing credentials, network access, versions and possible costs. Postgres, Kubernetes, external stores and public OIDC issuance SHALL not be prerequisites. Host AWS identity plus role assumption SHALL work without stored static keys; publish supported OS/architecture versions.

#### Scenario: Small installation
- **WHEN** a user follows the local quickstart on a supported clean machine
- **THEN** they can start the server, authenticate, configure a none/echo platform, deploy, inspect and safely remove a workload without cloud credentials

### Requirement: Exclusive ownership and shutdown

Exactly one engine SHALL own an installation's database/state at a time. Startup SHALL acquire exclusive ownership before accepting mutations and fail if another owner is active; selecting Postgres later SHALL not imply multi-writer support. Graceful shutdown SHALL stop accepting mutations, drain or cancel bounded local operations, checkpoint recovery metadata and report unresolved external effects. Restart SHALL follow the interrupted-operation contract in `deployments`.

#### Scenario: Second process
- **WHEN** a second server attempts to use an already-owned installation
- **THEN** it exits with a clear ownership error and never starts deployment work

### Requirement: Liveness and readiness

`/livez` SHALL report process liveness. `/readyz` SHALL return `200` only with usable database/schema, keys, exclusive ownership and executor; otherwise return `503` and non-sensitive reason codes. `/healthz` SHALL alias readiness. Optional cloud outages SHALL not block local readiness. Missing required prerequisites SHALL reject mutations; safe administrative recovery/read APIs MAY remain available.

#### Scenario: Database works but key is missing
- **WHEN** the database is reachable but an existing installation's master key is absent
- **THEN** `/livez` returns `200`, readiness returns `503` naming the key ID only, and no deployment is accepted

### Requirement: Backup, restore and upgrades

Documentation SHALL provide a quiesced consistent backup of database, local state, recovery records, source/locks and encryption/signing keys, and inventory needed external state/secret versions. Protect backups as secrets. Restore SHALL begin read-only and reconcile identities/state before mutations; stale observations SHALL not cause resource recreation. Releases SHALL follow the schema compatibility requirement below.

#### Scenario: Restore after total server loss
- **WHEN** a tested backup is restored to a fresh supported host with required keys and backend access
- **THEN** the resource inventory and state identities are recoverable, readiness waits for recovery checks, and the first redeployment does not duplicate existing cloud resources

#### Scenario: Unsupported downgrade
- **WHEN** an older binary opens a schema version outside its published compatibility range
- **THEN** it refuses mutations and leaves stored data unchanged

### Requirement: Uninstall and retention

Removing the server binary or installation SHALL NOT implicitly destroy cloud resources. Documentation SHALL separate uninstall, protected-data removal and explicit permitted infrastructure teardown. Recovery records, active-resource state and referenced artifacts/secrets SHALL not expire with logs. A maintenance command SHALL report retained objects, retention deadlines and potential continuing cost before deleting eligible history or temporary files.

#### Scenario: Uninstall with active resources
- **WHEN** an operator uninstalls the control plane
- **THEN** infrastructure remains and documented backup/export instructions allow management to be restored

### Requirement: Bounded work and storage

Publish and enforce configurable global driver concurrency (default 4), queue length, graph node/edge limits, request sizes and log quotas/retention. Throttling SHALL return `429` with `Retry-After`; queue/storage exhaustion SHALL reject work that cannot be persisted. Provisioning SHALL leave API capacity available. Metrics SHALL avoid unbounded resource-ID labels.

#### Scenario: Concurrent environments
- **WHEN** multiple environments each have four runnable OpenTofu nodes and the global limit is four
- **THEN** at most four OpenTofu processes run in total and status requests remain available

#### Scenario: Storage exhausted
- **WHEN** the configured log quota is reached
- **THEN** log truncation is explicitly signaled and operational metadata remains intact; if durable operation or audit records cannot be stored, new mutations are rejected

### Requirement: Reference capacity validation

Release validation SHALL benchmark 2 vCPUs/2 GiB RAM, a 100-node/1,000-edge graph, four delayed fake-driver jobs and 10 status requests/second. Over at least 100 samples, planning p95 SHALL be below 2 seconds and status API p95 below 500 ms, excluding external latency. Publish peak server/subprocess memory and disk growth. Capacity revisions require an explicit spec change; real cloud work uses deadlines, not this latency promise.

#### Scenario: Benchmark under driver load
- **WHEN** the release benchmark runs with the fixed graph and delayed drivers
- **THEN** its report records the configuration, sample count, latency percentiles and memory/disk results and fails the declared latency thresholds on regression

### Requirement: Observability and diagnosis

Structured events/logs SHALL correlate request, deployment, operation and GUResID. Metrics SHALL cover queued/active work, retries, outcomes, duration, recovery-required resources and storage pressure. Status SHALL timestamp observations and distinguish stale/unknown from healthy. Diagnostics SHALL contain versions, sanitized configuration and statuses, excluding tokens, secret files/values and raw state. External telemetry SHALL be optional and off by default.

#### Scenario: Diagnose a timeout
- **WHEN** an operator exports diagnostics for a timed-out deployment
- **THEN** they can correlate the operation and its recovery state without receiving secret values or raw provider state

### Requirement: Public compatibility and release hygiene

Releases SHALL publish supported API/workload/driver/provider versions, owned schemas/fixtures and a finite tested integration matrix. Unsupported versions SHALL fail clearly; persisted formats and migrations SHALL be versioned/tested. Breaking contracts require a new major version or explicit pre-1.0 migration guide. Release builds SHALL follow the reproducibility and contribution requirements below.

#### Scenario: Release from clean checkout
- **WHEN** CI builds a release from its tagged commit
- **THEN** locked dependency resolution, tests and schema fixtures pass, and downloadable artifacts have verifiable checksums linked to that commit

### Requirement: Schema compatibility

Releases SHALL declare readable/writable schema versions and test migration from the preceding supported release. Unsupported downgrades SHALL refuse mutations. Reinstalling an older binary is supported only for tested binary/schema pairs; otherwise rollback requires a compatible backup and recovery checks. Additive migrations alone SHALL not imply downgrade safety.

#### Scenario: Incompatible previous binary
- **WHEN** an operator reinstalls an older binary against a newer unsupported schema
- **THEN** startup refuses mutations and names the required recovery path

### Requirement: Reproducible release inputs

Builds SHALL use committed locks and pinned toolchains, with a documented clean build and CI-produced release checksums/provenance tied to the source commit. Reproducibility claims SHALL distinguish locked source inputs from byte-for-byte deterministic artifacts and state which was verified.

#### Scenario: Clean release build
- **WHEN** CI builds the same tagged source using its documented locked inputs
- **THEN** it verifies locks and schemas and publishes verifiable artifact checksums and source provenance

### Requirement: Open-source contribution and security policy

The repository SHALL provide Apache-2.0 licensing, contribution/test instructions, vulnerability reporting and a maintainer release checklist. A foundation, marketplace or enterprise certification is not a first-release requirement.

#### Scenario: Contributor follows release guidance
- **WHEN** a maintainer prepares a release using the checklist
- **THEN** the documented checks include tests, dependency/license review, compatibility notes and artifact verification
