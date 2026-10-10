# Spec Delta

## Purpose

The openporch workload spec is the single developer-facing format: one YAML document describes a workload as either containers or a function, with its triggers and resource dependencies, and Score files are accepted as an import so existing Score workloads keep working.

## ADDED Requirements

### Requirement: Workload document

A workload SHALL be a YAML document with `apiVersion: openporch.dev/v1`, `kind: Workload`, `metadata` (`name`, optional `class` defaulting to `default`, optional `annotations`), exactly one of `containers` or `function`, and optional `service`, `triggers`, `resources`, `deploy` and `target`. Unknown fields SHALL be rejected with an error naming the field and its path.

#### Scenario: Function workload
- **WHEN** a document declares `function` and `resources` and no `containers`
- **THEN** it is accepted as a function workload

#### Scenario: Both kinds
- **WHEN** a document declares both `containers` and `function`
- **THEN** it is rejected with an error stating a workload is either containers or a function

#### Scenario: Unknown field
- **WHEN** a document contains `volumes` at the top level
- **THEN** it is rejected with an error naming `volumes`

### Requirement: Container workloads

`containers` SHALL follow Score's container shape: a map of container ID to `image`, `command`, `args`, `variables`, `files` (content or source, mode), `resources` (limits and requests), `livenessProbe`, `readinessProbe`. `service.ports` SHALL follow Score's shape (`port`, `targetPort`, `protocol`) and implies an `http` trigger for the first port when `triggers` names none.

#### Scenario: Web container
- **WHEN** a document declares container `web` with an image and `service.ports.http` mapping 80 to 8080
- **THEN** the workload is a container workload with one implied `http` trigger on port `http`

### Requirement: Function workloads

`function` SHALL declare `package` (`type: zip` with `artifact`, `runtime`, `handler`, or `type: image` with `image`), `variables`, `memory` (quantity, default `256Mi`), `timeout` (duration, default `30s`), and optional `concurrency` (`reserved`, `provisioned`). `artifact` and `image` MAY be `.` to mean supplied at deploy time. No container is required.

#### Scenario: Zip function
- **WHEN** a document declares `function.package` with `type: zip`, `runtime: python3.12`, `handler: app.handler` and `artifact: .`
- **THEN** it is accepted, and deploying it requires `--artifact <name>=<uri>` or fails naming the missing artifact

#### Scenario: Image function
- **WHEN** a document declares `function.package` with `type: image` and `image: .`
- **THEN** it is accepted and deploying it requires `--image <name>=<ref>`

### Requirement: Triggers

`triggers` SHALL map names to one kind: `http` (port/path/auth), `schedule` (5-field cron/timezone/input), `queue` (resource/batchSize), `topic` (resource) or `stream` (resource/batchSize/startingPosition). Resource names SHALL exist under `resources`. Driver capabilities SHALL reject unsupported kinds/combinations before any mutation. HTTP auth values are `none|iam`; defaults/exposure follow target policy.

#### Scenario: Queue trigger on a dependency
- **WHEN** `triggers.inbox.queue.resource` is `inbox` and `resources.inbox` is an `sqs`
- **THEN** the workload depends on that queue in the graph and the target receives the trigger with the queue's outputs

#### Scenario: Trigger names a missing resource
- **WHEN** `triggers.inbox.queue.resource` is `inbox` and no `resources.inbox` exists
- **THEN** the document is rejected naming `inbox`

#### Scenario: Unsupported trigger for the target
- **WHEN** a `queue` trigger reaches a workload definition using `openporch/kubernetes`
- **THEN** preflight fails with an error naming the trigger and driver, before provisioning any dependency

### Requirement: Resource dependencies

Each entry under `resources` SHALL have `type`, optional `class` (default `default`), optional `id`, optional `params` and `metadata.annotations`. Without `id` the dependency is private with resource ID `modules.<workload>.externals.<key>`; with `id` it is shared with resource ID `shared.<id>`.

#### Scenario: Shared resource
- **WHEN** two workloads both declare `resources.dns` with `type: dns` and `id: api-dns`
- **THEN** both reference one resource `dns.default#shared.api-dns`

### Requirement: Placeholder translation

Placeholders `${resources.<key>.<output>}` in variables, files and trigger inputs SHALL be translated to set placeholders `${externals.<key>.<output>}` or `${shared.<id>.<output>}`. Outputs not in the type's schema SHALL be rejected at conversion time. A value containing a secret output SHALL be marked secret.

#### Scenario: Connection string
- **WHEN** a variable is `postgresql://${resources.db.username}:${resources.db.password}@${resources.db.host}:${resources.db.port}/${resources.db.name}`
- **THEN** the workload stores the translated placeholder string, resolves it at deploy time from the postgres outputs, and marks the variable secret because `password` is a secret output

#### Scenario: Unknown output
- **WHEN** a variable references `${resources.db.hostname}` and `postgres` has no `hostname` output
- **THEN** conversion fails naming `hostname`

### Requirement: Deploy settings and target passthrough

`deploy` MAY set status `deploy|available` (default available) and timeout (default 5m). `target` fragments SHALL satisfy the selected driver's schema and administrator policy; unknown/unsupported fields fail preflight. Workload input and stored specs SHALL reject `${context.*}` and use `${values.*}` for environment configuration; context placeholders remain allowed in platform definitions. `${pod.*}` requires supported target capability.

#### Scenario: Pod annotation
- **WHEN** `target.kubernetes.pod.annotations.billing-code` is `X` and the workload is deployed through the Kubernetes driver
- **THEN** the rendered pod template carries that annotation

### Requirement: Workload class selects the target

Each workload SHALL be a `workload` resource `workload.<class>#modules.<name>` matched to a resource definition like any other resource. The matched driver SHALL receive the fully resolved workload document (containers or function, triggers with their resources' outputs, service, deploy, target) as its resource inputs.

#### Scenario: Lambda class
- **WHEN** workload `api` has class `aws-lambda` and a `workload` definition with criteria `{class: aws-lambda}` uses the OpenTofu driver
- **THEN** that definition provisions the workload and its module receives the resolved document as inputs

#### Scenario: No workload definition
- **WHEN** no `workload` definition matches the workload's context
- **THEN** the deployment fails naming the workload, its class and the environment

### Requirement: Workload without a runtime target

A `workload` definition using driver `openporch/none` SHALL provision the workload without creating anything: the resolved document is stored as the workload's outputs with secret values tainted.

#### Scenario: Resolved outputs without a runtime
- **WHEN** a workload matched to an `openporch/none` definition has a `postgres` dependency and is deployed
- **THEN** the deployment succeeds, the postgres is provisioned, and the workload's active resource shows its variables with the database host filled in and the password marked secret

### Requirement: Score import

A document with `apiVersion: score.dev/v1b1` SHALL be accepted and converted to a container workload: `metadata.name`, `containers`, `service` and `resources` map one to one. An optional sidecar file with `apiVersion: openporch.dev/v1` and `kind: WorkloadOverlay` MAY supply `metadata.class`, `triggers`, `deploy` and `target`. Score fields openporch does not support SHALL be rejected naming the field.

#### Scenario: Plain Score file
- **WHEN** a valid Score file with one container and a `postgres` resource is deployed
- **THEN** the result equals the openporch document with the same containers and resources and class `default`

#### Scenario: Overlay adds a class
- **WHEN** a Score file is deployed with an overlay setting `metadata.class: aws-lambda`
- **THEN** the workload is matched with class `aws-lambda`

### Requirement: Deploy-time overrides

After conversion and before delta creation, apply `--image <container-or-function>=<ref>`, `--artifact <function>=<uri>` and `--property <path>=<value>`. Before acceptance, pin artifacts to S3 versions or verified immutable content-addressed objects and image tags to digests. Hash-shaped filenames alone SHALL not prove immutability. Store identities in snapshots and recheck at execution/replay; unresolvable artifacts SHALL fail before mutation.

#### Scenario: Override image from CI
- **WHEN** CI deploys with `--image web=ghcr.io/acme/web:abc123`
- **THEN** the delta's workload container `web` uses that image regardless of the file's `image`

#### Scenario: Mutable artifact
- **WHEN** CI deploys with `--artifact main=s3://acme-artifacts/order-worker/latest.zip` without a version ID
- **THEN** the command resolves and pins a version if the bucket is versioned, or fails explaining that an immutable object identity is required
