# Spec Delta

## Purpose

Defines the shape of the openporch control plane API that the CLI, the OpenTofu provider and CI systems call: a single organization, a uniform error format, identifier rules and health endpoints.

## ADDED Requirements

### Requirement: Single organization control plane

The orchestrator SHALL expose an HTTP/JSON API rooted at `/orgs/{org}` for exactly one organization whose ID is set in server configuration. Requests naming any other organization SHALL be rejected with `404`.

#### Scenario: Request for the configured organization
- **WHEN** the server is configured with org ID `acme` and a client calls `GET /orgs/acme/apps`
- **THEN** the server returns `200` with the list of applications

#### Scenario: Request for an unknown organization
- **WHEN** a client calls `GET /orgs/other/apps`
- **THEN** the server returns `404` with an error body

### Requirement: Authentication and authorization on every route

Every API route except health, version, login and OIDC discovery SHALL require an authenticated principal (see the `authentication` capability) and an explicit permission check (see the `rbac` capability). `401` means no valid credential; `403` means the principal lacks the permission.

#### Scenario: Unauthenticated request
- **WHEN** a request carries no valid credential
- **THEN** the server returns `401` and does not process the request

#### Scenario: Forbidden request
- **WHEN** an authenticated `viewer` deletes an application
- **THEN** the server returns `403`

### Requirement: Uniform error format

Every error response SHALL be a JSON object with `error` (a stable machine-readable code such as `RES-001`) and `message` (human-readable). Validation errors SHALL use `400`, missing objects `404`, and conflicts with current state `409`.

#### Scenario: Validation failure
- **WHEN** a client creates an application with ID `My App`
- **THEN** the server returns `400` and a body whose `error` code identifies invalid ID format and whose `message` explains the allowed pattern

### Requirement: Object identifiers

IDs of applications, environments, environment types, resource definitions, cloud accounts and classes SHALL match `^[a-z0-9](?:-?[a-z0-9]+)+$` with at most 50 characters, and SHALL be unique within their parent scope.

#### Scenario: Duplicate ID
- **WHEN** a client creates an application with an ID that already exists
- **THEN** the server returns `409`

### Requirement: Health and version endpoints

The server SHALL expose unauthenticated `GET /healthz` returning `200` when it can reach its database, and `GET /version` returning the build version.

#### Scenario: Healthy server
- **WHEN** a client calls `GET /healthz` on a running server with a reachable database
- **THEN** the server returns `200`
