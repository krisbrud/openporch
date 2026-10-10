# Spec Delta

## Purpose

A deployment attempts to converge an environment to a desired set. Desired configuration, successful deployment history and observed per-resource outcomes are distinct, because cloud mutations are not one transaction.

## ADDED Requirements

### Requirement: Triggering a deployment

`POST .../deploys` SHALL accept one of `delta_id`, `set_id`, `deploy_id`, optional comment/permits, `expected_generation` and `Idempotency-Key`. Atomically check permissions/generation and no conflicting mutation, freeze a delta, persist job/audit intent and desired set, then increment generation. Deltas apply to the desired set, initially empty. Return the job ID immediately; last-successful state changes only on success.

#### Scenario: Deploy a delta
- **WHEN** a client posts a valid delta and the current generation
- **THEN** one `in_progress` deployment exists, the delta is frozen, `desired_set_id` names its result, and `last_successful_set_id` is unchanged until success

#### Scenario: Stale or concurrent request
- **WHEN** a request names an old generation or another deployment or deletion is running in that environment
- **THEN** the server returns `409` without changing desired state or freezing the submitted delta

#### Scenario: Lost HTTP response
- **WHEN** a client retries an accepted request with the same idempotency key and body
- **THEN** it receives the same deployment ID and no second job is created

#### Scenario: Missing deployer role
- **WHEN** a developer without `deployer` on `production` submits a production deployment
- **THEN** the server returns `403` before accepting it

### Requirement: Deployment record and convergence

Each deployment SHALL record actor, set/delta IDs, comment, generation, status (`in_progress|succeeded|failed`), times and snapshot. Per resource retain attempts, referenced outputs, error, observation time and status (`pending|provisioning|provisioned|failed|skipped|unknown`). Success requires all provisions/removals and atomically sets last-successful deployment/set. Failure retains desired state and checkpoints; live state SHALL not be described as wholly old or new.

#### Scenario: Partial success
- **WHEN** a database update succeeds and a workload update fails
- **THEN** the database's successful result is retained, the previous successful deployment remains identified, and the environment reports partial convergence rather than claiming that the old set is still fully deployed

### Requirement: Retries and failure semantics

Retryable failures SHALL have at most three total attempts with jittered exponential backoff from 1 second capped at 30 seconds. Permanent auth/validation/capability errors do not retry. Honor rate-limit hints within the default 30-minute deployment deadline, including calls/polls/delays. Failed nodes leave dependents pending; independent branches continue. Deployments SHALL not restart automatically; uncertain effects require recovery before retry.

#### Scenario: One branch fails
- **WHEN** DNS exhausts three retryable attempts while the database branch succeeds
- **THEN** DNS is `failed`, its route is `pending`, the database is `provisioned`, and the deployment is `failed`

#### Scenario: Permanent driver error
- **WHEN** an HTTP driver returns a validation `400`
- **THEN** only one attempt is made

### Requirement: Durable operations and interrupted deployments

Before mutation the engine SHALL persist GUResID, operation ID, referenced inputs, dependencies and recovery metadata. Checkpoint cookies/results before releasing dependents. Restart SHALL fail interrupted deployments, preserve records and resolve uncertain operations using observation or the same idempotent identity. If neither establishes safety, mark `recovery_required` and block conflicting mutation until explicitly resolved. Whole-deployment auto-resume is not required.

#### Scenario: Crash after creation
- **WHEN** a cloud resource is created but the server crashes before persisting its result
- **THEN** recovery uses the persisted identity to find or safely repeat that operation, keeps the same GUResID, and never blindly creates a replacement

#### Scenario: Driver keeps running after timeout
- **WHEN** a request times out but a remote operation may still be executing
- **THEN** the resource is `unknown`, its recovery metadata is retained, and a new deployment cannot mutate it until the outcome is resolved

### Requirement: Execution snapshots

Snapshots SHALL retain definition content, driver/protocol version, module commit/locks, backend, edges, artifact identities, value versions and secret-reference versions, never plaintext. Capture configuration-bound versions at acceptance; checkpoint new output references before dependent use. Later edits SHALL not affect the accepted job. Missing versions fail explicitly. Snapshots support execution/recovery, not public definition version management; replay rechecks current policy.

#### Scenario: Configuration changes during execution
- **WHEN** a definition or shared value is edited after deployment acceptance
- **THEN** all nodes use the accepted configuration snapshot and a subsequent deployment uses the new configuration

### Requirement: Deployment modes

Mode `full` SHALL visit every resource in the desired graph, using the driver contract to converge without duplicate infrastructure. `incremental` SHALL return `400` in milestone 1. Reapplying identical inputs SHALL preserve resource identity and data unless an explicit destructive action has been authorized.

#### Scenario: Unchanged deployment
- **WHEN** an unchanged set and configuration are applied twice
- **THEN** the second deployment keeps GUResIDs and driver-managed state and introduces no duplicate objects

### Requirement: Redeploy and rollback

`deploy_id` selects its set; `+0` is the latest attempt, `+n` the nth earlier attempt including failures, and `last-successful` the latest success. Default `replay: snapshot` uses recorded configuration and SHALL reject unavailable/incomplete replay inputs before mutation. Explicit `replay: current` rematches definitions and current values. Neither restores data nor bypasses safety policy. CLI SHALL expose these references/modes.

#### Scenario: Recover after a failed deployment
- **WHEN** `porch deploy deploy last-successful --app shop --env dev` runs
- **THEN** a new attempt uses that successful deployment's recorded workload and configuration, subject to current permissions and safety policy

#### Scenario: Artifact no longer exists
- **WHEN** snapshot replay requires an artifact version that was removed
- **THEN** validation fails before infrastructure changes and identifies the missing artifact

### Requirement: Deployment history and logs

Deployment history SHALL be paginated newest first. Per-resource logs and events SHALL carry deployment, operation and resource IDs and SHALL be redacted before persistence or streaming. Terminal failures SHALL state whether effects are known, partially applied or uncertain and name the supported recovery action.

#### Scenario: Redacted failure
- **WHEN** an OpenTofu failure prints a known secret variable
- **THEN** persisted and streamed output contains `[REDACTED]` and the resource's recovery state, never that value

### Requirement: Idempotent submission

Keys SHALL be scoped to principal/app/env and retained at least as long as their deployment. Identical retries SHALL return the original result after current access checks, without another generation check or mutation; different content under the same key returns `409`. A retry SHALL not reveal a deployment after access is revoked.

#### Scenario: Retry after generation advanced
- **WHEN** a deployment acceptance response is lost and the authorized client repeats its key/body
- **THEN** the original deployment is returned despite the generation increment caused by that acceptance

### Requirement: Explicit convergence scope

Milestone 1 SHALL converge on explicit deployments using observations under the driver contract. Periodic drift detection is deferred. Read APIs SHALL expose observation timestamps and unknown outcomes; a previous success SHALL not imply that a later partially failed deployment left live infrastructure unchanged.

#### Scenario: Reading after partial failure
- **WHEN** one branch mutated infrastructure before another branch failed
- **THEN** the API returns partial convergence and both branches' recorded outcomes
