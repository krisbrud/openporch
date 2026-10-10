# Spec Delta

## Purpose

The `porch` command line is the day-to-day interface for developers and platform engineers, mirroring the vocabulary of the API and of `humctl`.

## ADDED Requirements

### Requirement: Configuration and authentication

`porch` SHALL read the server URL, organization and token from flags, environment variables (`OPENPORCH_URL`, `OPENPORCH_ORG`, `OPENPORCH_TOKEN`) and a config file in that precedence, and SHALL support `porch login --token` to store the token in the config file. Default `--app` and `--env` MAY be set in the config file.

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

`porch get|create|delete <kind>` SHALL exist for `apps`, `envs`, `env-types`, `deploys`, `deltas`, `sets`, `active-resources`, `resource-definitions`, `resource-types`, `resource-classes`, `cloud-accounts`, with `-o yaml|json|table`. `porch create resource-definition -f file.yaml` SHALL accept the YAML `kind: Definition` format.

#### Scenario: Apply a definition file
- **WHEN** `porch create resource-definition -f stg-redis.yaml` runs with a valid `kind: Definition` document
- **THEN** the definition exists on the server

### Requirement: Deployment commands

`porch deploy delta <id> <env>`, `porch deploy deploy <id|+n>`, `porch diff sets <a> <b>`, `porch get deploys`, `porch logs deploy <id>` and `porch resources graph --app --env` SHALL be provided.

#### Scenario: Follow logs
- **WHEN** `porch logs deploy <id>` runs for an in-progress deployment
- **THEN** it streams provisioning log lines until the deployment finishes

### Requirement: Raw API access

`porch api get|post|put|delete <path> [-d body]` SHALL send an authenticated request to the server and print the JSON response, so unsupported operations stay reachable.

#### Scenario: Raw get
- **WHEN** `porch api get /orgs/acme/resources/types` runs
- **THEN** the JSON list of resource types is printed
