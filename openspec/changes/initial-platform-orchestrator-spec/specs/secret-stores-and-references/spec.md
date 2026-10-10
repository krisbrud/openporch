# Spec Delta

## Purpose

Secret stores and secret references let platform engineers and developers use secrets without ever handling or seeing their values: the orchestrator stores references, writes new secrets into one primary store, and reads values only at provisioning time.

## ADDED Requirements

### Requirement: Secret stores

Secret stores SHALL be registered under `/orgs/{org}/secretstores` with an ID, a `type` and type-specific configuration. The built-in store `openporch` (encrypted at rest in the orchestrator database) SHALL always exist. Milestone 2 SHALL add `aws-secrets-manager` and `aws-ssm` stores configured with a region and a cloud account. Exactly one store SHALL be `primary`; the built-in store is primary until another is marked.

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

Only the executor SHALL resolve authorized secret references immediately before driver use; planning retains references/tainted expressions. Redact before logging, streaming or persistence. Required driver files/state SHALL follow the protected-artifact contract; other API/planner records retain references. APIs SHALL not return application secret values; specified one-time authentication-token issuance is separate.

#### Scenario: Secret output stored as a reference
- **WHEN** a driver returns a secret output `password`
- **THEN** the executor writes it to the primary store and the active resource records a reference and version, not the value

#### Scenario: Store unreachable
- **WHEN** a referenced store cannot be read during a deployment
- **THEN** the resource fails with an error naming the store and reference, not the value

### Requirement: Store lifecycle

A referenced store SHALL not be deleted. Bulk built-in secret deletion through the API is forbidden. Retain built-in versions referenced by active resources or retained snapshots. Externally supplied secrets SHALL never be deleted. Executor-written external secrets require an explicit ownership/retention policy for cleanup. Lambda module-owned SSM parameters follow that module's state/teardown policy, not external-store retention.

#### Scenario: Delete a store in use
- **WHEN** an administrator deletes `prod-sm` while a definition references it
- **THEN** the server returns `409` naming the definition

### Requirement: Secret-use grants

Reference possession SHALL not grant use. Administrator grants under `/orgs/{org}/secretstores/{store}/grants` name an exact reference, app and optional env; explicit org-wide scope MAY be used for platform definitions. Check assignment and execution against consuming scope, including replay. Grant removal SHALL block subsequent resolution. Workload authors can access secrets granted to their workloads; API redaction is not protection from them.

#### Scenario: Reference copied between applications
- **WHEN** a developer in `shop` submits a known `{store, ref}` belonging to `billing` without a grant to `shop`
- **THEN** assignment is denied and the executor cannot resolve it even if the store's cloud account could read it

#### Scenario: Production reference in development
- **WHEN** a production-only secret is submitted in a development environment
- **THEN** validation fails before a driver call

### Requirement: Protected artifacts and key lifecycle

Secret files SHALL be owner-only, absent from logs/diagnostics and cleaned after completion or safe restart recovery. Protect state, saved plans, backups, cookies and keys with documented encryption/access controls; remote state also requires verified TLS and least privilege. `sensitive` SHALL not mean encryption. Administrator acceptance is required for user-managed backends. Rotation/restore SHALL follow the key-recovery requirement below.

#### Scenario: Failure leaves a variables file
- **WHEN** an OpenTofu process crashes with a secret variables file on disk
- **THEN** only the server account can read it, recovery removes it when safe, and diagnostics omit its contents

#### Scenario: Master key unavailable
- **WHEN** a restored database references a master key that was not restored
- **THEN** readiness fails with the missing key ID and no deployment runs

### Requirement: Credential dependency validation

Secret-store and cloud-account configuration SHALL form an acyclic credential-resolution graph that terminates in a usable host identity or built-in secret. A store SHALL not require credentials available only through itself. Account/store deletion checks SHALL include grants, snapshots needed for active-resource recovery, and other stores/accounts, not only current resource definitions.

#### Scenario: Store depends on itself
- **WHEN** an AWS secret store uses an account whose key exists only in that same store
- **THEN** configuration validation rejects the credential cycle

### Requirement: Implicit object-scoped grants

Secrets written through `{value}` SHALL receive use grants only for their owning object's scope. Environment secrets SHALL not become app-wide grants; cross-scope reuse requires administrator approval. Definition secrets SHALL have explicit administrator-approved consumer scope, including explicit organization-wide scope if intended.

#### Scenario: Environment secret copied to app values
- **WHEN** a developer attempts to promote a production-only generated secret reference into application-wide values
- **THEN** the assignment fails without an explicit cross-scope grant

### Requirement: Key recovery and rotation

Master-key rotation SHALL preserve decryptability of referenced ciphertext and be verified by restore tests. An existing installation missing required keys SHALL fail readiness without silently generating replacements. Normal API responses and logs SHALL exclude application secret values, including on errors.

#### Scenario: Restore after key rotation
- **WHEN** a backup contains ciphertext referencing both retained old and new key IDs
- **THEN** restore decrypts each version with its recorded key and fails safely if a required key is absent
