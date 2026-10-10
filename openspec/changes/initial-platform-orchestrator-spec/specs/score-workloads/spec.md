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

An optional extension file with `apiVersion: openporch.dev/v1` MAY set `class` (the workload class, default `default`), `deploy` (`status: deploy|available`, `timeout`), and `spec` fragments that the workload target understands (for example `pod.annotations` for Kubernetes or `lambda.memory_mb` for Lambda). The file SHALL be environment-agnostic and SHALL NOT contain placeholders bound to a specific environment.

#### Scenario: Pod annotation
- **WHEN** the extension file sets `spec.pod.annotations.billing-code: X` for a workload whose target is Kubernetes
- **THEN** the rendered pod template carries that annotation

#### Scenario: Lambda memory
- **WHEN** the extension file sets `class: aws-lambda` and `spec.lambda.memory_mb: 512`
- **THEN** the workload resource is matched with class `aws-lambda` and its resolved inputs carry `spec.lambda.memory_mb: 512`

### Requirement: Workload class selects the target

Each workload SHALL be a `workload` resource `workload.<class>#modules.<id>` matched to a resource definition like any other resource, so the platform decides per class, application and environment whether a workload runs on Kubernetes, as an AWS Lambda function, or nowhere. The matched driver SHALL receive the fully resolved workload spec (containers, variables, files, service ports, extension `spec`) as its resource inputs.

#### Scenario: Lambda class
- **WHEN** workload `api` has class `aws-lambda` and a `workload` definition with criteria `{class: aws-lambda}` uses the OpenTofu driver
- **THEN** that definition provisions the workload and its module receives the resolved containers and variables as inputs

#### Scenario: No workload definition
- **WHEN** no `workload` definition matches the workload's context
- **THEN** the deployment fails naming the workload, its class and the environment

### Requirement: Workload without a runtime target

A `workload` definition using driver `openporch/none` SHALL provision the workload without creating anything: the resolved spec is stored as the workload's outputs with secret values tainted. It lets an environment's cloud resources be deployed and verified before any runtime exists.

#### Scenario: Resolved outputs without a runtime
- **WHEN** a workload matched to an `openporch/none` definition has a `postgres` dependency and is deployed
- **THEN** the deployment succeeds, the postgres is provisioned, and the workload's active resource shows its variables with the database host filled in and the password marked secret

### Requirement: Deploy properties and overrides

`porch score deploy` SHALL accept `--property path=value` overrides and `--image` to set a container image, applied after conversion and before the delta is created.

#### Scenario: Override image from CI
- **WHEN** CI deploys with `--image web=ghcr.io/acme/web:abc123`
- **THEN** the delta's workload container `web` uses that image regardless of the file's `image`
