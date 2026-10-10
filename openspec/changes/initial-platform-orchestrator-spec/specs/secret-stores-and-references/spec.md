# Spec Delta

## Purpose

Secret stores and secret references let platform engineers and developers use secrets without ever handling or seeing their values: the orchestrator stores references, writes new secrets into one primary store, and reads values only at provisioning time.

## ADDED Requirements

### Requirement: Secret stores

Secret stores SHALL be registered under `/orgs/{org}/secretstores` with an ID, a `type` and type-specific configuration. The built-in store `openporch` (encrypted at rest in the orchestrator database) SHALL always exist. Milestone 1 SHALL also support `aws-secrets-manager` and `aws-ssm` stores configured with a region and a cloud account. Exactly one store SHALL be `primary`; the built-in store is primary until another is marked.

#### Scenario: Register an AWS store
- **WHEN** an administrator registers `prod-sm` of type `aws-secrets-manager` with `region` and `cloud_account: aws-prod`
- **THEN** secret references may name store `prod-sm` and a store check reads a test secret with that account

#### Scenario: Change primary
- **WHEN** an administrator marks `prod-sm` primary
- **THEN** new secrets written through the orchestrator go to `prod-sm` and existing references are unchanged

### Requirement: Secret reference format

A secret reference SHALL be an object `{store, ref, version?}` where `ref` is the store-specific name (secret name for AWS Secrets Manager, parameter name for SSM, key for the built-in store) and `version` pins a version; without it the latest is read. References SHALL NOT contain placeholders.

#### Scenario: Pinned version
- **WHEN** a reference has `version: "3"`
- **THEN** that version is read even after the secret is rotated

#### Scenario: Placeholder in a reference
- **WHEN** a reference's `ref` contains `${context.env.id}`
- **THEN** the request is rejected explaining placeholders are not allowed in references

### Requirement: Where references are used

Secret references SHALL be accepted in resource definitions under `driver_inputs.secret_refs` (mirroring the structure of `values`), in cloud account credentials, in shared values (`secret_ref`), and in secret store configuration. The resolved values SHALL be merged into the driver's `secrets` at provisioning time.

#### Scenario: Definition with references from two stores
- **WHEN** `secret_refs.variables.username` names store `my-sm` and `secret_refs.variables.password` names store `openporch`
- **THEN** the OpenTofu run receives both as secret variables

### Requirement: Writing secrets through the orchestrator

A secret reference MAY be given as `{value}` instead of `{store, ref}`. The orchestrator SHALL write the value to the primary store under a generated name scoped to the object, replace the reference with `{store, ref, version}` and never return the value. Inline `driver_inputs.secrets` SHALL be accepted as sugar for this and stored the same way.

#### Scenario: Inline secret becomes a reference
- **WHEN** a definition is created with `driver_inputs.secrets.password: hunter2`
- **THEN** the stored definition has `secret_refs.password` pointing at the primary store and reads show the reference, not the value

#### Scenario: Update creates a version
- **WHEN** the same key is updated with a new `value`
- **THEN** a new version is written and the reference now pins it

### Requirement: Resolution and access

Only the executor component that invokes drivers SHALL read secret values, using the store's cloud account, immediately before the call; the planning side of a deployment SHALL handle references only. Values SHALL be held in memory for the duration of the call and redacted from logs. No API endpoint SHALL return a secret value.

#### Scenario: Secret output stored as a reference
- **WHEN** a driver returns a secret output `password`
- **THEN** the executor writes it to the primary store and the active resource records a reference and version, not the value

#### Scenario: Store unreachable
- **WHEN** a referenced store cannot be read during a deployment
- **THEN** the resource fails with an error naming the store and reference, not the value

### Requirement: Store lifecycle

A store SHALL NOT be deleted while any reference names it. Deleting the organization's secrets through the API SHALL be impossible for the built-in store; externally stored secrets are never deleted by the orchestrator.

#### Scenario: Delete a store in use
- **WHEN** an administrator deletes `prod-sm` while a definition references it
- **THEN** the server returns `409` naming the definition
