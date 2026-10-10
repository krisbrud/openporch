# Spec Delta

## Purpose

The openporch OpenTofu provider lets platform engineers manage the orchestrator's platform-side objects as infrastructure as code.

## Delivery scope

Milestone 1 includes the provider resources needed by the Lambda reference platform (see tasks 7.2–7.3). Broader custom-type, driver, secret-store and identity-provider resources follow in milestone 2. Unavailable server capabilities SHALL be rejected before mutation.

## ADDED Requirements

### Requirement: Provider configuration

The provider `openporch/openporch` SHALL be configured with `url`, `org_id` and `token`, each overridable by `OPENPORCH_URL`, `OPENPORCH_ORG` and `OPENPORCH_TOKEN`. It SHALL use plugin protocol version 6 and publish a finite tested OpenTofu compatibility range per release, rather than promising all future versions.

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

`driver_inputs` SHALL accept JSON-compatible values and sensitive secrets. Sensitivity redacts display, not stored state; require protected/encrypted provider state and prefer references. Reads SHALL compare reference versions and keys without retrieving values. Configured value changes SHALL plan updates; unchanged inputs SHALL give no-op plans.

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

### Requirement: Protected deletion and compatibility

Provider deletion SHALL obey server preview/permit checks, with explicit plan-fingerprint acknowledgement for infrastructure destruction and administrator permission for protected environments. A provider destroy SHALL not imply unconditional server authorization. Publish tested server/provider versions and import identifiers; acceptance tests SHALL cover them. One-time tokens in state SHALL follow protected-state rules.

#### Scenario: Accidental application removal from HCL
- **WHEN** a provider plan would delete an application with active resources but no matching destruction acknowledgement is configured
- **THEN** apply returns an actionable preview/permit error before deprovisioning infrastructure
