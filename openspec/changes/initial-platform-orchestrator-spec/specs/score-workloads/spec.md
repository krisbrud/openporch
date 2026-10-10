# Spec Delta

## Purpose

Developers describe a workload once in a Score file; the orchestrator turns it into a workload entry in a deployment delta, with its resource dependencies and placeholders.

## ADDED Requirements

### Requirement: Supported Score subset

The orchestrator SHALL accept `apiVersion: score.dev/v1b1` files with `metadata.name`, `containers` (image, command, args, variables, files with content or source, resources limits/requests, livenessProbe, readinessProbe), `service.ports`, and `resources` (type, class, id, params, metadata.annotations). Unsupported top-level fields SHALL be rejected with an error naming the field.

#### Scenario: Minimal workload
- **WHEN** a Score file declares one container with an image and a `postgres` resource `db`
- **THEN** conversion yields a workload named after `metadata.name` with that container and a private dependency `db` of type `postgres`

#### Scenario: Unsupported field
- **WHEN** a Score file contains the key `volumes` at the top level
- **THEN** conversion fails with an error naming `volumes`

### Requirement: Resource dependency mapping

Each entry under `resources` SHALL become a dependency with `type`, `class` (default `default`) and `params`. Without `id` the dependency is private with resource ID `modules.<workload>.externals.<key>`. With `id` it is shared with resource ID `shared.<id>`.

#### Scenario: Shared resource
- **WHEN** two workloads both declare `resources.dns` with `type: dns` and `id: api-dns`
- **THEN** both reference one resource `dns.default#shared.api-dns`

### Requirement: Score placeholder translation

Placeholders of the form `${resources.<key>.<output>}` in container variables and files SHALL be translated to deployment-set placeholders `${externals.<key>.<output>}` (private) or `${shared.<id>.<output>}` (shared). Outputs not in the type's schema SHALL be rejected at conversion time.

#### Scenario: Connection string
- **WHEN** a variable is `postgresql://${resources.db.username}:${resources.db.password}@${resources.db.host}:${resources.db.port}/${resources.db.name}`
- **THEN** the workload stores `postgresql://${externals.db.username}:...` and at deploy time resolves it from the provisioned postgres outputs, marking the variable as a secret because `password` is a secret output

#### Scenario: Unknown output
- **WHEN** a variable references `${resources.db.hostname}` and `postgres` has no `hostname` output
- **THEN** conversion fails naming `hostname`

### Requirement: Extension file

An optional extension file with `apiVersion: openporch.dev/v1` MAY set `profile` (default `openporch/default`), `deploy` (`status: deploy|available`, `timeout`), and `spec` fragments (pod annotations, labels, replicas, service account) merged into the workload. The file SHALL be environment-agnostic and SHALL NOT contain placeholders bound to a specific environment.

#### Scenario: Pod annotation
- **WHEN** the extension file sets `spec.pod.annotations.billing-code: X`
- **THEN** the rendered pod template carries that annotation

### Requirement: Deploy properties and overrides

`porch score deploy` SHALL accept `--property path=value` overrides and `--image` to set a container image, applied after conversion and before the delta is created.

#### Scenario: Override image from CI
- **WHEN** CI deploys with `--image web=ghcr.io/acme/web:abc123`
- **THEN** the delta's workload container `web` uses that image regardless of the file's `image`
