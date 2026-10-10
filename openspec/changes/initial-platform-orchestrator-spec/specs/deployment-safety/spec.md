# Spec Delta

## Purpose

Provide a small, server-enforced safety contract for validation, destructive changes and ownership without requiring an enterprise approval service.

## Delivery scope

Milestone 1: preflight and preview, destructive-action permits, ownership and conflict checks, trusted execution, unknown preview effects, permit binding and driver transport scope. Protected environment types are first-release hardening after milestone 1 (tasks group 14); until they exist, permit issuance follows the operation's application and environment type permissions alone.

## ADDED Requirements

### Requirement: Preflight and preview

`POST .../envs/{env}/plans` SHALL validate a candidate set/delta and expected generation, match definitions, build the graph, and check schemas, capabilities, reference grants, artifacts and target policy without provisioning. Persist a preview ID and per-resource create/update/delete/replace/no-op/unknown actions with reasons. Direct deploy calls SHALL perform the same validation; a set diff SHALL not claim to be a complete infrastructure plan.

#### Scenario: Unsupported target
- **WHEN** a Kubernetes workload requests a queue trigger and a database
- **THEN** preflight rejects the trigger before the database or any other resource is provisioned

### Requirement: Destructive-action permits

Delete, replace, detach and unknown-impact mutations SHALL require a permit from `POST .../envs/{env}/plans/{plan}/permits`, scoped to resources/actions, org/app/env, base generation, configuration and available driver-plan fingerprints. Permits expire after 1 hour and authorize only one matching operation and its idempotent retries. Changed inputs, state or policy invalidate unused permits. Known non-destructive operations MAY proceed without permits.

#### Scenario: Database replacement
- **WHEN** an OpenTofu plan contains database replacement but only an ordinary update was permitted
- **THEN** apply does not run and the deployment records `approval_required` with the affected resource and preview

#### Scenario: Inputs become resolvable later
- **WHEN** earlier dependencies succeed and a newly available driver plan reveals an unapproved destructive action
- **THEN** that resource is not mutated, the deployment fails with `approval_required`, completed dependencies remain tracked, and the user can preview and explicitly authorize a new attempt

### Requirement: Permission and protection policy

Permit issuance SHALL require the operation's app/type permissions; detach/migration additionally require administrator. Only administrators may mark environment types `protected`; protected destructive permits require administrator and unknown-impact changes are denied until planned. Definitions, accounts, grants and target policy remain centrally managed. CLI/API SHALL not bypass checks. Separate approvers and a general policy engine are deferred.

#### Scenario: Protected environment
- **WHEN** a developer with production deployment permission requests a permit to delete a database in a protected environment
- **THEN** the request returns `403` unless the principal is also an administrator

### Requirement: Ownership and conflict checks

Creation SHALL record external identity/ownership evidence; matching names or labels SHALL not imply adoption. Administrator adoption SHALL record identity, other-owner checks, management boundaries and retain/delete policy without creating the object. Field/state-lock conflicts SHALL fail without force until explicitly resolved. Milestone 1 rejects driver/backend migration but SHALL export recovery information for operator-led migration.

#### Scenario: Pre-existing namespace
- **WHEN** the desired namespace already exists without recorded ownership or adoption
- **THEN** the orchestrator refuses to take ownership and environment deletion does not remove it

### Requirement: Trusted execution and target restrictions

Administrators, drivers and modules SHALL be treated as trusted code with executor-host privileges; milestone 1 does not support hostile-plugin/tenant isolation. Run the server unprivileged. Module inputs SHALL reject traversal/symlink escapes. Developer target fragments SHALL obey driver schemas and admin policy; executable module selection, privileged pods, host mounts, role escalation and cluster-scoped manifests are denied by default.

#### Scenario: Developer requests host access
- **WHEN** a workload target fragment requests a privileged pod or host filesystem mount without an administrator allowance
- **THEN** validation fails before any driver mutation

#### Scenario: File escapes the run directory
- **WHEN** a module input names `../../server.db` or a symlink outside the working directory
- **THEN** the driver rejects the input before writing the file

### Requirement: Unknown preview effects

Inputs requiring unprovisioned outputs SHALL be marked unresolved. Drivers without plan support SHALL report unknown impact. Once inputs become available, known effects SHALL be reclassified and checked against permits before mutation; newly known destruction SHALL not be hidden by a generic unknown acknowledgement.

#### Scenario: Dependency output reveals replacement
- **WHEN** a previously unresolved driver input now produces a replacement plan
- **THEN** the resource waits for a matching replacement permit rather than consuming an unknown-impact acknowledgement

### Requirement: Permit binding at acceptance

Acceptance SHALL validate permits against the expected base generation and bind them to accepted operation IDs before incrementing generation. That increment alone SHALL not invalidate them. Execution SHALL recheck current authorization, relevant policy/state and inputs; changed policy or revoked authority blocks execution. An expired unconsumed permit SHALL not authorize a new operation.

#### Scenario: Generation increments after acceptance
- **WHEN** an authorized deployment consumes a matching permit while atomically advancing generation
- **THEN** its bound operation can execute without treating its own generation increment as a stale-plan conflict

### Requirement: Driver transport and credential scope

External destinations SHALL require administrator registration and verified TLS; redirects SHALL not forward credentials across origins. Executor jobs SHALL receive only their required account credentials and secret inputs, and workload identities SHALL be scoped to their declared dependencies. Environment filtering is not a sandbox against trusted module code.

#### Scenario: External driver redirects credentials
- **WHEN** a registered HTTPS driver responds with a redirect to another origin
- **THEN** the client rejects credential forwarding and records a redacted transport failure
