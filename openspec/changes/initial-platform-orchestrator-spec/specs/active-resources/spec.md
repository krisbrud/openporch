# Spec Delta

## Purpose

Active resources are the concrete things the orchestrator has provisioned in an environment, tracked with a stable identity so drivers can manage their state across deployments.

## ADDED Requirements

### Requirement: Identity

Each active resource SHALL have a permanent, non-semantic GUResID allocated and durably recorded before the first provisioning call and stable across deployments, plus its descriptor `<type>.<class>#<res_id>` which is unique within an environment. The GUResID SHALL be what drivers and the OpenTofu state store are keyed by.

#### Scenario: Stable across deployments
- **WHEN** `postgres.default#modules.web.externals.db` is provisioned in two consecutive deployments
- **THEN** both runs use the same GUResID and the same OpenTofu state

### Requirement: Record contents

An active resource SHALL retain descriptor, GUResID, definition/driver version, non-secret inputs/outputs, secret references, encrypted cookie, operation/recovery metadata, execution snapshot, backend and external object identities, ownership, dependency edges, last successful deployment, observation time, status and `scheduled_deletion`. It SHALL be checkpointed per operation and survive partial failure.

#### Scenario: Read outputs
- **WHEN** a client reads an active postgres resource
- **THEN** it sees `host`, `port`, `name` values and the names of secret outputs but not their values

### Requirement: Unreferenced resources persist

Removed non-workload dependencies SHALL remain tracked as retained and potentially billable for later reuse. Removed workloads SHALL lose their owned runtimes/triggers on every target under `deployment-safety`; dependencies remain unless separately scheduled for deletion. Retained records and destroy dependencies SHALL NOT expire with deployment logs.

#### Scenario: Dependency removed then restored
- **WHEN** a workload drops its `db` dependency in one deployment and adds it back in the next
- **THEN** the second deployment reuses the existing active resource and its data

### Requirement: Deprovisioning rules

Owned resources SHALL be deprovisioned on permitted environment deletion, permitted workload removal, or permitted deployment excluding a resource scheduled for deletion. Deletion SHALL use retained source/state and reverse retained dependency order, including unreferenced resources. Remove the live record only on confirmed deletion and keep an audit tombstone. Adopted resources default to retain; external resources SHALL never be destroyed.

#### Scenario: Scheduled deletion
- **WHEN** a client schedules `postgres/modules.web.externals.db` via `DELETE .../envs/{env}/resources/{type}/{res_id}` and then deploys a set without it with a matching destructive-action permit
- **THEN** the driver's `delete` runs during that deployment and the record disappears

### Requirement: Listing

`GET /orgs/{org}/apps/{app}/envs/{env}/resources` SHALL list active resources with usage (which deployment last provisioned them, and whether the current graph references them). `GET /orgs/{org}/resources/defs/{def}/resources` SHALL list active resources across environments for a definition.

#### Scenario: Usage after a partial failure
- **WHEN** the current deployment failed to provision `postgres` but succeeded one deployment ago
- **THEN** the listing shows `postgres` with usage `1 deploy ago`

### Requirement: Detach

Detach SHALL require administrator permission, a reason, a matching permit and no running/uncertain operation. It removes management without calling the driver and retains an exportable tombstone with object IDs, GUResID and state location. Audit SHALL record actor and reason. A detached descriptor SHALL not silently receive a fresh GUResID and recreate its object; later adoption is explicit.

#### Scenario: Detach
- **WHEN** an administrator detaches an inactive resource with a matching permit and reason
- **THEN** the record is gone, the driver was not called, and the real-world object still exists

### Requirement: Partial deletion recovery

Deletion SHALL persist each completed and failed step. A failed step SHALL block deletion of resources it depends on, preserve remaining records and state, and allow an explicit retry using the same identities. Failure to remove state after confirmed cloud deletion SHALL be reported as cleanup pending and retried without recreating the cloud object.

#### Scenario: Database deletion fails
- **WHEN** workload deletion succeeds but database deletion fails
- **THEN** the database and its prerequisites remain tracked, the environment remains in deletion-failed state, and retry continues safely without repeating an already completed cloud deletion

### Requirement: Driver and backend changes

A definition change SHALL NOT implicitly destroy, replace or migrate an active resource when its driver, backend or ownership target changes. Milestone 1 SHALL return `409` and retain management metadata for an explicit operator-led migration or separately permitted replacement.

#### Scenario: Definition switches backend
- **WHEN** an in-use definition is changed to another backend
- **THEN** the next deployment fails with a migration-required conflict without deleting the old resource or state
