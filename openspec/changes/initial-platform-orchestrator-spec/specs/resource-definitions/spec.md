# Spec Delta

## Purpose

A resource definition tells the orchestrator how to provision one resource type (which driver, with which inputs) and when that definition applies (matching criteria).

## ADDED Requirements

### Requirement: Definition structure

A resource definition SHALL have an `id`, `name`, `type` (an existing resource type), `driver_type`, optional `driver_account` (a cloud account ID or `${resources...account}` placeholder), `driver_inputs` with `values`, `secret_refs` and (on write) `secrets` maps, optional `provision` map, and `criteria`. It SHALL be accepted as JSON on the API and as YAML with `kind: Definition` on the CLI.

#### Scenario: Create a definition over the API
- **WHEN** a client `POST`s a definition `stg-redis` of type `redis` with `driver_type: openporch/opentofu` and criteria `[{env_type: staging}]`
- **THEN** the definition is stored and returned by `GET /orgs/{org}/resources/defs/stg-redis`

#### Scenario: Unknown driver
- **WHEN** a client creates a definition whose `driver_type` is not registered
- **THEN** the server returns `400` naming the driver

### Requirement: Driver input secrets are references

Secret driver inputs SHALL be given under `driver_inputs.secret_refs` as secret references (`{store, ref, version?}` or `{value}` to write into the primary store), mirroring the structure of `values`. Inline `driver_inputs.secrets` SHALL be accepted and converted to references on write. Reads SHALL show the references and never a value.

#### Scenario: Read a definition with secrets
- **WHEN** a client reads a definition created with `driver_inputs.secrets.password`
- **THEN** the response shows `secret_refs.password` as `{store: openporch, ref: ..., version: ...}` and no value

#### Scenario: External reference
- **WHEN** a definition sets `secret_refs.variables.db_password` to `{store: prod-sm, ref: shop/db-password}`
- **THEN** provisioning reads that secret from `prod-sm` and passes it to the driver as a secret variable

### Requirement: Matching criteria and specificity

Each criterion SHALL contain any subset of `env_type`, `app_id`, `env_id`, `res_id`, `class`. A definition with no criteria SHALL never match. For a resource context, the matching criterion with the highest weight wins, with weights env_type 1, app_id 2, env_id 4, res_id 8, class 16. Ties SHALL fail the deployment with an ambiguity error.

#### Scenario: More specific criterion wins
- **WHEN** `static-dev-dns` matches `{app_id: shop, env_id: dev}` and `random-dns` matches `{env_type: development}` and a `dns` resource is resolved in `shop/dev`
- **THEN** `static-dev-dns` is selected

#### Scenario: No match
- **WHEN** a `postgres` resource is resolved and no definition matches its context
- **THEN** the deployment fails with an error naming the type, class, resource ID and context

#### Scenario: Ambiguous match
- **WHEN** two definitions of type `s3` both match a context with identical weight
- **THEN** the deployment fails with an ambiguity error naming both definitions

### Requirement: Criteria management

Criteria SHALL be addable and removable individually (`POST`/`DELETE /orgs/{org}/resources/defs/{def}/criteria[/{id}]`) so the OpenTofu provider can manage each as its own resource. Removing a criterion SHALL NOT affect already active resources until the next deployment.

#### Scenario: Add a criterion
- **WHEN** a client adds criterion `{env_type: production, class: large}` to `prod-postgres`
- **THEN** the definition lists it with a generated criterion ID

### Requirement: Co-provisioning

A definition MAY declare `provision`, mapping type to `{is_dependent, match_dependents, params}`. It SHALL add that type with the source class/ID and resolved params. Flags default false; edges SHALL follow this table. For source A, co-resource B and existing consumer C, X -> Y means X waits for Y.

| is_dependent | match_dependents | Edges |
| --- | --- | --- |
| false | false | B -> A |
| false | true | B -> A, C -> B |
| true | false | A -> B |
| true | true | A -> B, C -> B |

#### Scenario: Policy co-provisioned with a database
- **WHEN** the `postgres` definition provisions `aws-policy` with `is_dependent: false, match_dependents: true` and the `aws-policy` definition references `${resources.postgres.outputs.name}`
- **THEN** the graph contains an `aws-policy` with the database's resource ID that is provisioned after the database, and workloads consuming the database wait for both and receive the policy output

### Requirement: Deleting a definition in use

A definition SHALL NOT be deleted while active resources were provisioned by it. The API SHALL return `409` listing the environments that use it.

#### Scenario: Delete in use
- **WHEN** a client deletes `prod-postgres` while `shop/production` has a postgres provisioned by it
- **THEN** the server returns `409` naming `shop/production`

### Requirement: Co-provisioning expansion

Consumers SHALL include workload/resource-reference dependencies but exclude the co-resource itself. Expansion SHALL reach a deterministic fixed point and deduplicate edges. Placeholder edges are additional; cycles fail preflight. `match_dependents: true` exposes the co-resource as a direct dependency/output of each consumer. Repeated descriptors SHALL have identical canonical inputs; conflicts fail preflight.

#### Scenario: Policy edge required by workload
- **WHEN** a table co-provisions a policy with `match_dependents: true`
- **THEN** the workload waits for both table and policy and receives both outputs regardless of traversal order
