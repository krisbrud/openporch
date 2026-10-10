# Spec Delta

## Purpose

The `porch` command line is the day-to-day interface for developers and platform engineers, mirroring the vocabulary of the API and of `humctl`.

## ADDED Requirements

### Requirement: Configuration and authentication

`porch` SHALL read the server URL, organization and token from flags, environment variables (`OPENPORCH_URL`, `OPENPORCH_ORG`, `OPENPORCH_TOKEN`) and a config file in that precedence, and SHALL support `porch login` (device-flow login through the server's identity provider, storing a session token) and `porch login --token` to store an API token. Default `--app` and `--env` MAY be set in the config file.

#### Scenario: Environment variables
- **WHEN** `OPENPORCH_URL`, `OPENPORCH_ORG` and `OPENPORCH_TOKEN` are set and `porch get apps` runs
- **THEN** it lists the organization's applications

### Requirement: Workload deploy

`porch workload deploy -f workload.yaml --app A --env E [--overlay file] [--image name=ref] [--artifact name=uri] [--property path=value] [--comment text] [--wait]` SHALL accept an openporch workload document or a Score file, convert it, create a delta, trigger a deployment and, with `--wait`, follow it until it finishes and exit non-zero on failure. `porch score deploy` SHALL be an alias that requires a Score file.

#### Scenario: Deploy and wait
- **WHEN** a developer runs `porch workload deploy -f workload.yaml --app shop --env dev --wait`
- **THEN** the command prints the deployment ID, streams per-resource status lines, and exits 0 when the deployment succeeds

#### Scenario: Score alias
- **WHEN** a developer runs `porch score deploy -f score.yaml --app shop --env dev`
- **THEN** the Score file is imported and deployed exactly as `porch workload deploy` would

### Requirement: Object commands

`porch get` SHALL exist for `apps`, `envs`, `env-types`, `deploys`, `deltas`, `sets`, `active-resources`, `resource-definitions`, `resource-types`, `resource-classes`, `cloud-accounts`, with `-o yaml|json|table`. Create/delete verbs SHALL be offered only where the corresponding API lifecycle permits them; immutable sets and deployment records SHALL not gain arbitrary deletion or mutation verbs. `porch create resource-definition -f file.yaml` SHALL accept the YAML `kind: Definition` format.

#### Scenario: Apply a definition file
- **WHEN** `porch create resource-definition -f stg-redis.yaml` runs with a valid `kind: Definition` document
- **THEN** the definition exists on the server

### Requirement: Deployment commands

`porch deploy delta <id> <env>`, `porch deploy deploy <id|+n|last-successful> [--replay snapshot|current]`, `porch diff sets <a> <b>`, `porch get deploys`, `porch logs deploy <id>` and `porch resources graph --app --env` SHALL be provided.

#### Scenario: Follow logs
- **WHEN** `porch logs deploy <id>` runs for an in-progress deployment
- **THEN** it streams provisioning log lines until the deployment finishes

### Requirement: Membership and role commands

`porch users` (`list`, `invite`, `create-service`, `set-role`, `remove`, `tokens create|revoke`), `porch apps users <app> grant|revoke`, `porch env-types users <type> grant|revoke`, `porch secretstores` and `porch audit-logs` SHALL be provided with the same permission rules as the API.

#### Scenario: Create CI token
- **WHEN** an administrator runs `porch users tokens create ci --id deploy --expires-in 90d`
- **THEN** the token value is printed once with its expiry

### Requirement: Raw API access

`porch api get|post|put|delete <path> [-d body]` SHALL send an authenticated request to the server and print the JSON response, so unsupported operations stay reachable.

#### Scenario: Raw get
- **WHEN** `porch api get /orgs/acme/resources/types` runs
- **THEN** the JSON list of resource types is printed

### Requirement: Preview, explicit acknowledgement and recovery guidance

`porch workload plan` SHALL show server actions, unknown effects, app/environment and required permissions. Deploy/delete commands SHALL acknowledge named actions using preview fingerprints; generic `--yes` SHALL not bypass permits. Failures SHALL distinguish retry, approval and recovery required with operation/resource IDs. CLI SHALL expose `last-successful` and `--replay snapshot|current`, with `+n` counting attempts as the API does.

#### Scenario: Stale confirmation
- **WHEN** a user confirms a preview after the environment or relevant driver state changes
- **THEN** the server rejects its permit, the CLI requests a fresh preview and no destructive apply runs

#### Scenario: Interrupted deployment
- **WHEN** a waited deployment ends with an uncertain external operation
- **THEN** the CLI exits non-zero and identifies the resource and recovery action rather than suggesting an unconditional retry

### Requirement: Safe transport retries

Mutating CLI commands SHALL send expected generations and idempotency keys, retaining the key and request content across transport retries. A changed request SHALL use a new key and refreshed generation.

#### Scenario: Response lost after acceptance
- **WHEN** the CLI loses a deployment acceptance response and retries
- **THEN** the original deployment ID is returned rather than creating another job
