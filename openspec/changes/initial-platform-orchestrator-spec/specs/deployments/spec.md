# Spec Delta

## Purpose

A deployment applies a set to an environment: it builds the resource graph, provisions every resource and records the outcome, keeping a full history per environment.

## ADDED Requirements

### Requirement: Triggering a deployment

`POST /orgs/{org}/apps/{app}/envs/{env}/deploys` SHALL accept `{delta_id, comment}` or `{set_id, comment}`. With a delta, the orchestrator SHALL apply it to the environment's current set (empty if none) and deploy the result. The response SHALL return the deployment ID immediately while provisioning runs in the background.

#### Scenario: Deploy a delta
- **WHEN** a client posts a delta ID that adds workload `web` with a postgres dependency
- **THEN** a deployment is created with status `in_progress`, the delta is frozen, and the environment's current set becomes the delta's result

#### Scenario: Concurrent deployment
- **WHEN** a deployment to `shop/dev` is `in_progress` and another is requested
- **THEN** the server returns `409`

### Requirement: Deployment record

Each deployment SHALL record who triggered it, when, the `set_id`, the `delta_id` if any, the `comment`, `status` (`in_progress`, `succeeded`, `failed`), start and end times, and a per-resource list with resource descriptor, `status` (`pending`, `provisioning`, `provisioned`, `failed`, `skipped`), attempts and error message.

#### Scenario: Read deployment detail
- **WHEN** a client reads a finished deployment
- **THEN** every graph node appears with its final status and any error text

### Requirement: Retries and failure semantics

Each resource SHALL be attempted up to three times. A failed resource SHALL leave its dependents `pending` and SHALL NOT stop unrelated graph sections. The deployment SHALL be `failed` if any resource fails or the deployment exceeds its timeout (default 30 minutes), and SHALL never be retried automatically.

#### Scenario: One branch fails
- **WHEN** the dns resource fails three times while the postgres branch succeeds
- **THEN** dns is `failed`, the route depending on it is `pending`, postgres is `provisioned`, and the deployment is `failed`

### Requirement: Deployment modes

Deployments SHALL support mode `full` (every resource in the graph is provisioned, the default). Requests for mode `incremental` SHALL be rejected with `400` in this milestone.

#### Scenario: Full mode re-provisions
- **WHEN** a set is deployed again with no changes in `full` mode
- **THEN** every resource's driver is invoked again and each resource is re-associated with the new deployment

### Requirement: Redeploy and rollback

`POST .../deploys` SHALL accept `{deploy_id}` or a relative reference `+n` to deploy the set of an earlier deployment. The CLI SHALL expose this as `porch deploy deploy <id|+n>`.

#### Scenario: Roll back one deployment
- **WHEN** `porch deploy deploy +1 --app shop --env dev` runs after a failed deployment
- **THEN** a new deployment is created whose set ID equals that of the previous successful deployment

### Requirement: Deployment history and logs

`GET .../deploys` SHALL list deployments newest first, and `GET .../deploys/{id}/logs` SHALL stream per-resource provisioning logs with secrets redacted.

#### Scenario: Redacted logs
- **WHEN** an opentofu run prints a sensitive variable value
- **THEN** the stored log shows `[REDACTED]` in its place
