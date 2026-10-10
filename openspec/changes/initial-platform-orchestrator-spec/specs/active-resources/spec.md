# Spec Delta

## Purpose

Active resources are the concrete things the orchestrator has provisioned in an environment, tracked with a stable identity so drivers can manage their state across deployments.

## ADDED Requirements

### Requirement: Identity

Each active resource SHALL have a permanent, non-semantic GUResID generated at first provisioning and stable across deployments, plus its descriptor `<type>.<class>#<res_id>` which is unique within an environment. The GUResID SHALL be what drivers and the OpenTofu state store are keyed by.

#### Scenario: Stable across deployments
- **WHEN** `postgres.default#modules.web.externals.db` is provisioned in two consecutive deployments
- **THEN** both runs use the same GUResID and the same OpenTofu state

### Requirement: Record contents

An active resource SHALL record its descriptor, GUResID, definition ID, driver type, resolved non-secret inputs, non-secret outputs, secret outputs as secret references into the primary store (never returned as values), driver cookie, the ID of the deployment that last provisioned it successfully, status, and `scheduled_deletion`.

#### Scenario: Read outputs
- **WHEN** a client reads an active postgres resource
- **THEN** it sees `host`, `port`, `name` values and the names of secret outputs but not their values

### Requirement: Unreferenced resources persist

Removing a dependency from a set SHALL NOT deprovision its active resource. The resource remains, no longer referenced by the current graph, so a rollback can reuse it.

#### Scenario: Dependency removed then restored
- **WHEN** a workload drops its `db` dependency in one deployment and adds it back in the next
- **THEN** the second deployment reuses the existing active resource and its data

### Requirement: Deprovisioning rules

An active resource SHALL be deprovisioned when its environment is deleted, when it is `scheduled_deletion` and a later deployment does not include it, or when its definition's driver type changes. Deprovisioning calls the driver's `delete` and removes the record on success.

#### Scenario: Scheduled deletion
- **WHEN** a client deletes `postgres/modules.web.externals.db` via `DELETE .../envs/{env}/resources/{type}/{res_id}` and then deploys a set without it
- **THEN** the driver's `delete` runs during that deployment and the record disappears

### Requirement: Listing

`GET /orgs/{org}/apps/{app}/envs/{env}/resources` SHALL list active resources with usage (which deployment last provisioned them, and whether the current graph references them). `GET /orgs/{org}/resources/defs/{def}/resources` SHALL list active resources across environments for a definition.

#### Scenario: Usage after a partial failure
- **WHEN** the current deployment failed to provision `postgres` but succeeded one deployment ago
- **THEN** the listing shows `postgres` with usage `1 deploy ago`

### Requirement: Detach

`DELETE ...?detach=true` SHALL remove the active resource record without calling the driver, as an emergency fallback, and SHALL log a warning naming the resource.

#### Scenario: Detach
- **WHEN** a client detaches an active resource
- **THEN** the record is gone, the driver was not called, and the real-world object still exists
