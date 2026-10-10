# Spec Delta

## Purpose

The OpenTofu driver provisions any resource type by running OpenTofu code from a Git repository or an inline script with credentials from a cloud account, which is how AWS resources are created.

## ADDED Requirements

### Requirement: Driver inputs

`openporch/opentofu` SHALL accept values `source` (`url`, `rev`, `path`, `username`), `script` (inline HCL, written as `override.tf` when combined with `source`), `variables`, `files`, `credentials_config`, `backend` and `append_logs_to_error`, and secrets `variables`, `files`, `source.password`, `source.ssh_key`, and `tokens`. At least one of `source` or `script` SHALL be present.

#### Scenario: Inline script
- **WHEN** a definition provides only `script` declaring an `aws_s3_bucket` and an `output "bucket"`
- **THEN** a run creates the bucket and the resource outputs contain `bucket`

#### Scenario: Neither source nor script
- **WHEN** a definition provides neither
- **THEN** creating the definition fails with `400`

### Requirement: Run procedure

For each `put` the driver SHALL prepare an isolated working directory, clone `source` at `rev` (or write `script`), write `files`, configure state, run `tofu init` and `tofu apply -auto-approve -input=false` with `variables` passed as `-var` values (secrets through a variables file not logged), and return the module's outputs. The `tofu` binary and version SHALL be configurable.

#### Scenario: Git module with variables
- **WHEN** `source.url` is a repository containing an RDS module and `variables.instance_class` is `db.t4g.micro`
- **THEN** the run applies that module with that variable and returns outputs `host`, `port`, `name`, `username`, `password`

### Requirement: Credentials config

`credentials_config` SHALL map cloud account credential fields to the run as `environment` (env var names), `file` (a JSON file path exposed as a variable), or `variables` (OpenTofu variable names). Without `credentials_config` and with a `driver_account` of type `aws`/`aws-role`, the driver SHALL export `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN` and `AWS_REGION`.

#### Scenario: Default AWS environment
- **WHEN** a definition sets `driver_account: aws-dev` and no `credentials_config`
- **THEN** the AWS provider in the module authenticates through the exported environment variables

### Requirement: State handling

If the code declares a backend, the driver SHALL use it. Otherwise the driver SHALL store state in the orchestrator's state store keyed by GUResID: an S3 bucket configured on the server, or the local filesystem for development. A `local` backend in the code SHALL be ignored in favour of the orchestrator store.

#### Scenario: Orchestrator-managed state
- **WHEN** the server is configured with `state.s3_bucket` and a module without a backend is applied twice
- **THEN** the second apply reads the state written by the first and reports no changes

### Requirement: Outputs and secrets

Every OpenTofu output SHALL become a resource output; outputs marked `sensitive = true` SHALL become secret outputs. Output names SHALL be checked against the resource type's output schema.

#### Scenario: Sensitive output
- **WHEN** the module declares `output "password" { sensitive = true }`
- **THEN** `password` is a secret output and is redacted in logs

### Requirement: Deprovisioning

`delete` SHALL run `tofu destroy -auto-approve` against the same source revision and state, then remove the orchestrator-managed state. Repeated deletes after success SHALL be no-ops.

#### Scenario: Environment deleted
- **WHEN** an environment with an opentofu-provisioned bucket is deleted
- **THEN** `tofu destroy` removes the bucket and the state object is deleted

### Requirement: Failure reporting

On a non-zero exit the driver SHALL fail the attempt with the last lines of OpenTofu's stderr; with `append_logs_to_error: true` the full log is attached. Full logs SHALL always be available through the deployment logs endpoint.

#### Scenario: Apply fails
- **WHEN** `tofu apply` exits non-zero because of a missing IAM permission
- **THEN** the resource error contains the AWS error message and the deployment shows the full run log
