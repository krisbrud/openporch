# Spec Delta

## Purpose

Shared values and secrets give an application configuration that every workload can reference, with per-environment overrides, without putting environment-specific values into workload documents.

## ADDED Requirements

### Requirement: Values at application and environment level

Values SHALL be managed under `/orgs/{org}/apps/{app}/values` and `/orgs/{org}/apps/{app}/envs/{env}/values` with a `key`, `description` and either `value` or a secret (`secret_ref` or `{value}` to write). An environment value SHALL require an application value with the same key and overrides it.

#### Scenario: Environment override
- **WHEN** `API_URL` is set at application level and overridden in `production`
- **THEN** a deployment to `production` resolves `${values.API_URL}` to the override and other environments to the base value

#### Scenario: Override without base
- **WHEN** an environment value is created for a key that has no application value
- **THEN** the server returns `400`

### Requirement: Secret values

Secret values SHALL be stored as secret references and never returned; reads show the reference and version. A workload placeholder referencing a secret value SHALL make the containing variable secret and SHALL be rejected when mixed with other text.

#### Scenario: Secret in a composite string
- **WHEN** a variable is `Bearer ${values.API_TOKEN}` and `API_TOKEN` is secret
- **THEN** conversion fails explaining a secret placeholder must be the whole value

### Requirement: Value set versions

Every change to an application's or environment's values SHALL create a value set version. Deployments SHALL record the version they resolved, and `GET .../values/versions` SHALL list versions newest first.

#### Scenario: Deployment pins its version
- **WHEN** a value changes after a deployment
- **THEN** the deployment's record still names the earlier version and redeploying `+0` resolves the new one

### Requirement: Lifecycle

Application values SHALL be deleted with the application and environment values with the environment; secrets they wrote to the built-in store are deleted with them, and externally referenced secrets are left untouched.

#### Scenario: Delete environment
- **WHEN** `shop/pr-42` with secret overrides is deleted
- **THEN** its overrides are gone and the application values remain
