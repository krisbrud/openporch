# Spec Delta

## Purpose

A driver turns a resource definition's inputs into a provisioned resource and its outputs; drivers are the extension point for every kind of infrastructure.

## ADDED Requirements

### Requirement: Driver contract

Drivers SHALL implement GUResID-keyed idempotent `put` (converge), `get` (observe) and `delete`, with value/secret outputs, optional manifests and a versioned opaque cookie up to 10 KB. Poll asynchronous operations within deadlines. Mutations carry durable operation ID/input fingerprint; identical retries recover the same effect, changed fingerprints fail. Engine SHALL serialize operations per GUResID and checkpoint cookies for later calls.

#### Scenario: Asynchronous provisioning
- **WHEN** `put` returns `in_progress` with an estimated completion time
- **THEN** the orchestrator polls `get` and marks the resource `provisioned` when `get` returns outputs

#### Scenario: Cookie round trip
- **WHEN** a driver returns cookie `abc` from `put`
- **THEN** the next `put` or `delete` for the same GUResID receives cookie `abc`

### Requirement: Driver registry

Drivers SHALL use `<namespace>/<name>` IDs (`openporch` reserved for built-ins). Registry records SHALL pin protocol/cookie versions, input/output schemas and resource/workload/trigger, observation, recovery, preview and cancellation capabilities. External registration requires an HTTPS URL and optional secret bearer token. Incompatible definitions SHALL fail preflight. Releases SHALL ship built-in schemas and protocol fixtures.

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

External drivers SHALL use PUT/GET/DELETE `/{guresid}` with the documented Humanitec 0.9.3-derived profile: request `{type, resource, driver: {values, secrets}}`, response `{id, type, resource: {values, secrets}, manifests}`. Cookies use `Set-Openporch-Driver-Cookie`/`Openporch-Driver-Cookie`; mutations use `Openporch-Operation-Id`/`Openporch-Input-Fingerprint`. Fixtures SHALL specify version differences; full wire compatibility is not promised.

#### Scenario: Driver error
- **WHEN** an external driver returns `400` with `{error: "RES-101", message: "..."}`
- **THEN** the resource fails without retry and the message is shown in the deployment

### Requirement: Driver timeouts

Each driver call SHALL have a timeout (default 15 minutes, overridable per definition under `driver_inputs.values.timeout`). The deadline SHALL be bounded by the remaining deployment deadline. A timeout SHALL first cancel and reap a local subprocess group, or invoke supported remote cancellation and observe its result. A failed cancellation or uncertain remote outcome SHALL be recorded as `unknown` and recovered before retry. Timeout alone SHALL NOT authorize a second mutation.

#### Scenario: Hung driver
- **WHEN** a driver does not answer within the timeout
- **THEN** the timeout is recorded, cancellation/recovery is attempted, and no retry runs concurrently with an unresolved operation

### Requirement: Preview and driver upgrades

Drivers MAY offer read-only `plan` returning classified actions and input/state fingerprint; unsupported plans SHALL report unknown impact. Echo/none report no infrastructure effects; OpenTofu SHALL plan. Cookie upgrades require tested migration/compatible readers, otherwise activation fails for affected resources. Old driver/source metadata needed for recovery/deletion SHALL remain available.

#### Scenario: Incompatible cookie upgrade
- **WHEN** a driver upgrade cannot read an active resource's cookie version
- **THEN** activation is rejected with the affected GUResIDs and existing management metadata remains intact

#### Scenario: Duplicate put
- **WHEN** the same operation ID and inputs reach a driver twice after a response is lost
- **THEN** the driver returns or recovers the same object identity rather than creating another object

### Requirement: Observation and recovery capability

`get` SHALL distinguish absent, in-progress, ready, failed and unknown and return observation/recovery metadata. Drivers unable to recover ambiguous outcomes SHALL advertise that limitation; the engine SHALL block automatic mutation retries until an operator establishes a safe outcome.

#### Scenario: Unobservable timed-out creation
- **WHEN** an external creation times out and its driver cannot observe or deduplicate it
- **THEN** the engine marks recovery required and makes no second mutation automatically

### Requirement: HTTP status mapping

The adapter SHALL handle 200/202/204 completion/polling, permanent 400 validation failure, delete 404 as already absent, and 409 as a conflict requiring observation. Retry 429/transient 5xx only within deadline and with safe operation identity. Legacy drivers without recovery/preview guarantees SHALL report unknown capabilities and require matching policy.

#### Scenario: Conflict after a lost response
- **WHEN** an external PUT retry returns 409
- **THEN** the engine observes the prior operation instead of blindly repeating mutation
