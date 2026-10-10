# Spec Delta

## Purpose

The resource catalog names the kinds of things a workload can depend on (resource types with input and output schemas) and the flavours of each kind (resource classes).

## ADDED Requirements

### Requirement: Built-in resource types

The orchestrator SHALL ship with built-in resource types compatible in ID, use (direct, indirect, implicit), inputs and outputs with Humanitec's reference. Milestone 1 SHALL include at least: `workload`, `k8s-cluster`, `k8s-namespace`, `base-env`, `config`, `dns`, `route`, `postgres`, `postgres-instance`, `mysql`, `redis`, `s3`, `sqs`, `sns-topic`, `dynamodb-table`, `aws-role`, `aws-policy`, `k8s-service-account`.

#### Scenario: List types
- **WHEN** a client calls `GET /orgs/{org}/resources/types`
- **THEN** every built-in type is returned with its `use`, `category`, `inputs_schema` and `outputs_schema` (values and secrets)

#### Scenario: Workload type
- **WHEN** a client reads type `workload`
- **THEN** its inputs are `id`, `class`, `spec` (the resolved workload document: containers or function, triggers, service, deploy, target) and `dependencies` (outputs of its direct dependencies), and its outputs schema is open so targets can publish what they create, such as `url`

#### Scenario: Postgres outputs
- **WHEN** a client reads type `postgres`
- **THEN** its output values are `host`, `name`, `port` and its output secrets are `username`, `password`

### Requirement: Custom resource types

Clients SHALL be able to create resource types with an ID prefixed by the org ID (`{org}/{name}`), a `use`, an inputs JSON Schema and outputs JSON Schema. Built-in types SHALL NOT be modifiable or deletable.

#### Scenario: Create a custom type
- **WHEN** a client creates type `acme/feature-flags` with outputs schema `{sdk_key: secret string}`
- **THEN** resource definitions may target `acme/feature-flags` and workloads may request it under `resources`

#### Scenario: Modify a built-in type
- **WHEN** a client tries to update type `postgres`
- **THEN** the server returns `409`

### Requirement: Resource classes

A resource class SHALL be defined per resource type with an ID and description. Class `default` SHALL exist implicitly for every type. A class SHALL NOT be deleted while a resource definition's criteria reference it.

#### Scenario: Request an undefined class
- **WHEN** a workload requests `s3` with class `sensitive` and no such class exists for `s3`
- **THEN** the deployment fails at graph construction with an error naming the missing class

### Requirement: Output schema enforcement

When a driver returns outputs for a resource, the orchestrator SHALL verify that every output named in the type's output schema is present and of the declared kind (value or secret). Extra outputs are allowed but SHALL NOT be usable from workload documents.

#### Scenario: Missing required output
- **WHEN** the definition for `postgres` returns no `host`
- **THEN** provisioning of that resource fails with an error naming the missing output

### Requirement: Published schema contracts

Every built-in type SHALL ship versioned, pinned-dialect JSON Schema fixtures for required/optional inputs/outputs, types and sensitivity. External compatibility SHALL name a fixed version and differences. Schema changes follow public compatibility policy and SHALL not reinterpret stored inputs silently. Derived cloud names SHALL respect target limits, using deterministic collision-resistant shortening where needed.

#### Scenario: Catalog schema fixture
- **WHEN** a release validates its built-in catalog
- **THEN** every listed type has a schema fixture and missing required or incorrectly typed outputs fail its conformance tests
