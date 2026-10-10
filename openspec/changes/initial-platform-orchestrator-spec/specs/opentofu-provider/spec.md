# Spec Delta

## Purpose

The openporch OpenTofu provider lets platform engineers manage the orchestrator's platform-side objects as infrastructure as code.

## ADDED Requirements

### Requirement: Provider configuration

The provider `openporch/openporch` SHALL be configured with `url`, `org_id` and `token`, each overridable by `OPENPORCH_URL`, `OPENPORCH_ORG` and `OPENPORCH_TOKEN`. It SHALL target OpenTofu 1.6 or later through the plugin protocol version 6.

#### Scenario: Configure from environment
- **WHEN** a configuration declares the provider with no arguments and the three environment variables are set
- **THEN** `tofu plan` authenticates and succeeds

### Requirement: Platform object resources

The provider SHALL offer resources `openporch_application`, `openporch_environment_type`, `openporch_environment`, `openporch_resource_type`, `openporch_resource_class`, `openporch_resource_definition`, `openporch_resource_definition_criteria`, `openporch_cloud_account`, `openporch_driver` and `openporch_value`, each mapping one-to-one onto the corresponding API object with full create, read, update, delete and import support.

#### Scenario: Definition with criteria
- **WHEN** a configuration declares an `openporch_resource_definition` for `redis` with `driver_type = "openporch/opentofu"` and an `openporch_resource_definition_criteria` with `env_type = "staging"`
- **THEN** `tofu apply` creates both, and `tofu plan` afterwards shows no changes

#### Scenario: Import
- **WHEN** `tofu import openporch_application.shop shop` runs for an existing application
- **THEN** the state contains the application and a subsequent plan shows no changes

### Requirement: Driver inputs and secrets in HCL

`openporch_resource_definition` SHALL take `driver_inputs` as an object with `values` (arbitrary JSON-encodable map) and `secrets` (sensitive map). Secrets SHALL be marked sensitive in state and plans, and since the API never returns them, the provider SHALL detect drift only through the set of secret keys.

#### Scenario: Placeholder escaping
- **WHEN** a value is written `"$${context.app.id}-$${context.env.id}"` in HCL
- **THEN** the definition stored on the server contains `${context.app.id}-${context.env.id}`

### Requirement: Shared values

`openporch_value` SHALL manage an application-level or environment-level key with `value` or `secret_value`, exposed to workloads as `${values.<key>}` after resolution.

#### Scenario: Environment override
- **WHEN** `API_URL` is set at application level and overridden in `production`
- **THEN** a deployment to `production` resolves `${values.API_URL}` to the override

### Requirement: No ephemeral objects

The provider SHALL NOT offer resources for deployments, deltas or active resources; those are created through the CLI or API.

#### Scenario: Reviewer looks for a deploy resource
- **WHEN** a user searches the provider schema for a deployment resource
- **THEN** none exists and the documentation points to `porch score deploy`
