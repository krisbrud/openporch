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

### Requirement: Resource inputs

`openporch/opentofu` SHALL pass a resource's inputs (the params a workload gives it, or a `workload` resource's resolved spec and dependency outputs) to the module as the OpenTofu variable `resource`, holding the inputs without the values that resolved from secrets, and `resource_secrets`, holding those values alone at the same paths, which SHALL be written only to the secret variable file and redacted from logs. Each SHALL be set only when the resource has such inputs. A definition SHALL NOT set variables with either name.

#### Scenario: Table key from params
- **WHEN** a workload gives a `dynamodb-table` `params.primaryKey: {name: id, type: S}` and the module reads `var.resource.primaryKey`
- **THEN** the run applies the module with that key

#### Scenario: Secret workload variable
- **WHEN** a Lambda workload's variable `DB_PASSWORD` resolves from a postgres secret output and its variable `TABLE` is plain
- **THEN** the module receives `TABLE` under `var.resource.spec.function.variables` and `DB_PASSWORD` only under `var.resource_secrets.spec.function.variables`

#### Scenario: Reserved variable name
- **WHEN** a definition sets `driver_inputs.values.variables.resource`
- **THEN** creating the definition fails with `400`

### Requirement: Run procedure

For `put`, prepare a restricted run directory, resolve source to an immutable commit or write the script, and generate protected input files. Run noninteractive init and a saved plan; inspect actions/permits before applying that exact plan. Stale state SHALL require re-planning. Checkpoint result/state before exposing outputs. Configurable OpenTofu versions SHALL stay within the support matrix and preserve dependency integrity/TLS verification.

#### Scenario: Git module with variables
- **WHEN** `source.url` is a repository containing an RDS module and `variables.instance_class` is `db.t4g.micro`
- **THEN** the run applies that module with that variable and returns outputs `host`, `port`, `name`, `username`, `password`

### Requirement: Credentials config

`credentials_config` SHALL map cloud account credential fields to the run as `environment` (env var names), `file` (a JSON file path exposed as a variable), or `variables` (OpenTofu variable names). Without `credentials_config` and with a `driver_account` of type `aws`/`aws-role`, the driver SHALL export `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN` and `AWS_REGION`.

#### Scenario: Default AWS environment
- **WHEN** a definition sets `driver_account: aws-dev` and no `credentials_config`
- **THEN** the AWS provider in the module authenticates through the exported environment variables

### Requirement: State handling

Approved non-local module backends SHALL satisfy state separation, locking and protected-artifact policy. Override module-local backends with stable GUResID-keyed orchestrator state: configured S3 or persistent local state. Local state SHALL have owner-only permissions and documented encrypted-volume/state encryption. S3 is optional. Retain backend identity; definition edits SHALL not move it silently or make different GUResIDs share a state key.

#### Scenario: Orchestrator-managed state
- **WHEN** the server is configured with `state.s3_bucket` and a module without a backend is applied twice
- **THEN** the second apply reads the state written by the first and reports no changes

### Requirement: Outputs and secrets

Every OpenTofu output SHALL become a resource output; outputs marked `sensitive = true` SHALL become secret outputs. Output names SHALL be checked against the resource type's output schema.

#### Scenario: Sensitive output
- **WHEN** the module declares `output "password" { sensitive = true }`
- **THEN** `password` is a secret output and is redacted in logs

### Requirement: Deprovisioning

`delete` SHALL plan destruction against the retained source revision, dependency lock information and backend identity, validate its permit, and apply that exact destroy plan. The orchestrator-managed state SHALL be removed only after confirmed destruction. Missing source, expired credentials or failed destruction SHALL preserve state and recovery metadata. Repeated deletes after success SHALL be no-ops.

#### Scenario: Environment deleted
- **WHEN** an environment with an owned opentofu-provisioned bucket is deleted with a matching permit
- **THEN** `tofu destroy` removes the bucket and the state object is deleted

### Requirement: Failure reporting

On a non-zero exit the driver SHALL fail the attempt with the last lines of OpenTofu's stderr; with `append_logs_to_error: true` the full log is attached. Full logs SHALL always be available through the deployment logs endpoint.

#### Scenario: Apply fails
- **WHEN** `tofu apply` exits non-zero because of a missing IAM permission
- **THEN** the resource error contains the AWS error message and the deployment shows the full run log

### Requirement: State and process recovery

Mutations SHALL use backend locking and preserve partial-apply state outside ephemeral directories. Timeout SHALL terminate/reap the local process group before another invocation. Recovery SHALL inspect persisted state/provider observations; process termination does not undo cloud effects. Missing state or unresolved remote effects SHALL block mutation with actionable recovery instructions, never bypass locks or allocate fresh state.

#### Scenario: Partial apply writes state
- **WHEN** an apply creates one object and fails on a second
- **THEN** retry uses the same state and GUResID and converges the existing object instead of allocating a fresh state path

#### Scenario: Missing state
- **WHEN** an active resource's previously written backend state is unexpectedly absent
- **THEN** the driver reports `recovery_required` instead of treating the resource as a new installation

### Requirement: Retained dependency inputs

Resolve and retain module commit, provider/module selections and lockfile checksums. Sources needed for recovery/deletion SHALL remain retrievable; retrieval failure SHALL preserve state and fail explicitly. Secret variables SHALL use owner-only files, never command-line arguments or logged content.

#### Scenario: Module revision removed upstream
- **WHEN** recovery cannot obtain its retained source from cache or configured source
- **THEN** it fails without deleting state or substituting the current branch tip

### Requirement: Backend credential separation

Orchestrator-managed S3 state SHALL use a separately configured backend identity, defaulting to the server's identity scoped to the state bucket. Resource provisioning SHALL use the definition's cloud account. Supported backend role/configuration mechanisms SHALL keep the identities distinct without logging credentials or granting every resource account access to all state.

#### Scenario: State and resource accounts differ
- **WHEN** a definition provisions in an account that cannot read the control-plane state bucket
- **THEN** its resource provider uses that account while the backend uses its separately authorized state identity
