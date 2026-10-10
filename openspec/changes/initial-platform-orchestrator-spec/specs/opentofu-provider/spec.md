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

The provider SHALL offer one resource per platform object, each mapping one-to-one onto the API object with full create, read, update, delete and import support: `openporch_application`, `openporch_environment_type`, `openporch_environment`, `openporch_resource_type`, `openporch_resource_class`, `openporch_resource_definition`, `openporch_resource_definition_criteria`, `openporch_cloud_account`, `openporch_driver`, `openporch_value` and `openporch_secret_store`.

#### Scenario: Secret store
- **WHEN** a configuration declares `openporch_secret_store` of type `aws-ssm`
- **THEN** `tofu apply` registers it and a later plan shows no changes

### Requirement: Identity and access resources

The provider SHALL also offer `openporch_user` (service users), `openporch_service_user_token`, `openporch_application_user`, `openporch_environment_type_user` and `openporch_identity_provider`, with the same lifecycle support.

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

### Requirement: Secrets in provider resources

`openporch_value`, `openporch_cloud_account`, `openporch_secret_store` and `openporch_resource_definition` SHALL accept secrets either as a sensitive `value` (written to the primary store) or as a `secret_ref` block (`store`, `ref`, `version`). Written values SHALL be tracked by the reference and version the server returns, so a changed value plans an update and an unchanged one plans nothing.

#### Scenario: Secret value changed
- **WHEN** the sensitive `value` of an `openporch_value` changes in HCL
- **THEN** `tofu plan` shows an update and `apply` writes a new version

#### Scenario: Service user token
- **WHEN** a configuration declares `openporch_service_user_token` for user `ci` with `expires_at`
- **THEN** the token value is in state as sensitive and is never re-readable from the server

### Requirement: No ephemeral objects

The provider SHALL NOT offer resources for deployments, deltas or active resources; those are created through the CLI or API.

#### Scenario: Reviewer looks for a deploy resource
- **WHEN** a user searches the provider schema for a deployment resource
- **THEN** none exists and the documentation points to `porch score deploy`
