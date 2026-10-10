# Spec Delta

## Purpose

Cloud accounts hold the credentials the orchestrator uses to reach a cloud, so that resource definitions reference an account by ID instead of embedding keys.

## ADDED Requirements

### Requirement: AWS cloud account types

Milestone 1 SHALL support administrator-managed accounts under `/orgs/{org}/resources/accounts`: `aws` (static keys as secret references) and `aws-role` (AssumeRole from host identity, role ARN, required external ID and optional session duration). `aws-identity` SHALL be reserved for the later cloud OIDC integration; unavailable account types return `400` naming the missing capability.

#### Scenario: Create a static account
- **WHEN** a client creates account `aws-dev` of type `aws` with `access_key_id` and `secret_access_key`
- **THEN** the account exists, the secret is written to the primary secret store, and reads show a reference and never the value

#### Scenario: Create a role account
- **WHEN** a client creates account `aws-prod` of type `aws-role` with `role_arn` and `external_id`
- **THEN** the account exists and credential checks assume that role with the external ID

### Requirement: Account check

`POST /orgs/{org}/resources/accounts/{id}/check` SHALL obtain credentials for the account and call AWS STS `GetCallerIdentity`, returning the identity ARN on success or an error on failure.

#### Scenario: Wrong external ID
- **WHEN** the role's trust policy requires a different external ID
- **THEN** the check returns an error containing the STS failure message

### Requirement: Credential injection into drivers

When a definition sets `driver_account`, the orchestrator SHALL obtain temporary credentials for that account at provisioning time and pass them to the driver as `AccessKeyId`, `SecretAccessKey`, `SessionToken` (empty for static keys) and `Region`. Credentials SHALL NOT be stored in active resources or logs.

#### Scenario: OpenTofu driver with a role account
- **WHEN** a `postgres` definition uses `driver_account: aws-prod` and the opentofu driver
- **THEN** the run receives short-lived credentials for the assumed role, and the deployment log shows neither keys nor tokens

### Requirement: Deleting an account in use

A cloud account SHALL NOT be deleted while a definition, secret store, active resource or retained recovery snapshot requires it. The API SHALL return `409` naming the definitions.

#### Scenario: Delete in use
- **WHEN** `aws-prod` is referenced by `prod-postgres`
- **THEN** deleting `aws-prod` returns `409` naming `prod-postgres`

### Requirement: Credential lifetime and scope

The executor SHALL obtain credentials immediately before use with lifetime sufficient for the bounded operation, refresh through a supported provider mechanism, or fail safely before starting work that cannot complete before expiry. It SHALL never persist temporary credentials in execution snapshots. Reference modules SHALL scope IAM actions and resources to declared dependencies; wildcards SHALL require documented provider necessity or explicit administrator policy.

#### Scenario: Expiring role session
- **WHEN** available credentials expire before the operation's required window and cannot be refreshed
- **THEN** the executor obtains a new session or fails before mutation without logging credentials
