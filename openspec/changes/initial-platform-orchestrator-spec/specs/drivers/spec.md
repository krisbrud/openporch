# Spec Delta

## Purpose

A driver turns a resource definition's inputs into a provisioned resource and its outputs; drivers are the extension point for every kind of infrastructure.

## ADDED Requirements

### Requirement: Driver contract

Every driver SHALL implement three operations keyed by GUResID: `put` (create or update to the desired inputs, returning outputs `values`/`secrets` and optional `manifests`), `get` (current state), and `delete` (idempotent deprovision). Drivers MAY return `in_progress` and the orchestrator SHALL poll `get` until done or timed out. Drivers MAY persist an opaque cookie up to 10 KB that the orchestrator stores per active resource and returns on later calls.

#### Scenario: Asynchronous provisioning
- **WHEN** `put` returns `in_progress` with an estimated completion time
- **THEN** the orchestrator polls `get` and marks the resource `provisioned` when `get` returns outputs

#### Scenario: Cookie round trip
- **WHEN** a driver returns cookie `abc` from `put`
- **THEN** the next `put` or `delete` for the same GUResID receives cookie `abc`

### Requirement: Driver registry

Drivers SHALL be identified as `<namespace>/<name>`. Built-in drivers use namespace `openporch`. `GET /orgs/{org}/resources/drivers` SHALL list registered drivers with their input schemas. External drivers SHALL be registrable with an HTTPS URL and optional static bearer token under `/orgs/{org}/resources/drivers`.

#### Scenario: Register an external driver
- **WHEN** a client registers `acme/vault-secret` with `target: https://driver.internal/vault`
- **THEN** definitions may set `driver_type: acme/vault-secret` and provisioning calls that URL

### Requirement: Echo driver

`openporch/echo` SHALL return every `driver_inputs.values` entry as an output value and every `driver_inputs.secrets` entry as an output secret, without external calls. It SHALL be the way to represent externally managed resources.

#### Scenario: Static database instance
- **WHEN** a `postgres-instance` definition uses echo with values `host`, `port`, `name` and secrets `username`, `password`
- **THEN** the resource is provisioned instantly with exactly those outputs

### Requirement: None driver

`openporch/none` SHALL return the resource's resolved resource inputs as output values (secret-tainted inputs as output secrets) and create nothing. It is intended for `workload` definitions in environments that have no runtime yet, and for any resource that only needs to carry data through the graph.

#### Scenario: Workload without a runtime
- **WHEN** a `workload` definition uses `openporch/none`
- **THEN** provisioning succeeds immediately and the workload's outputs equal its resolved spec

### Requirement: Template driver

`openporch/template` SHALL render Jinja2-style templates from `driver_inputs.values.templates` with keys `init`, `outputs`, `secrets`, `manifests`, and optionally `cookie`, against `.driver`, `.resource`, `.init`, `.cookie` and `.guresid`. Rendered `outputs`/`secrets` YAML become the resource outputs; `manifests` become Kubernetes manifests with a `location`.

#### Scenario: Deterministic password
- **WHEN** the `init` template generates a password and stores it in `cookie`
- **THEN** a second deployment returns the same password from the cookie instead of a new one

### Requirement: External HTTP driver protocol

External drivers SHALL be called with `PUT`, `GET` and `DELETE /{guresid}` and JSON bodies compatible with Humanitec's Driver API 0.9.3: request `{type, resource, driver: {values, secrets}}`, response `{id, type, resource: {values, secrets}, manifests}`, status codes 200/202/204/400/404/409, and `Set-Openporch-Driver-Cookie` / `Openporch-Driver-Cookie` headers.

#### Scenario: Driver error
- **WHEN** an external driver returns `400` with `{error: "RES-101", message: "..."}`
- **THEN** the resource fails without retry and the message is shown in the deployment

### Requirement: Driver timeouts

Each driver call SHALL have a timeout (default 15 minutes, overridable per definition under `driver_inputs.values.timeout`). A timed-out call counts as a failed attempt.

#### Scenario: Hung driver
- **WHEN** a driver does not answer within the timeout
- **THEN** the attempt is recorded as failed with a timeout error
