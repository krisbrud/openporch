# Spec Delta

## Purpose

Applications group workloads that are deployed together; environments are the places an application instance runs, each of exactly one environment type that resource definitions can match on.

## ADDED Requirements

### Requirement: Application lifecycle

Applications SHALL support create/read/list/delete under `/orgs/{org}/apps`. Creation also creates environment `development` of type `development`; that default environment is exempt from the deployer gate, so creating an application needs no environment type role. The creator becomes owner. Application deletion SHALL require permits for all environments and deprovision their owned resources first; failed environments and remaining management records SHALL be preserved.

#### Scenario: Create an application
- **WHEN** a principal permitted to create applications creates application `shop`
- **THEN** `shop` exists, has one environment `development` with type `development`, and the creating principal is its `owner`

#### Scenario: Delete an application with provisioned resources
- **WHEN** an authorized client deletes an application with matching permits for its active resources
- **THEN** owned resources are deprovisioned before application removal, while external or retained adopted objects remain and are listed in the deletion result

### Requirement: Environment types

The orchestrator SHALL let clients create, list and delete environment types under `/orgs/{org}/env-types`. The type `development` SHALL exist by default. An environment type SHALL NOT be deleted while an environment uses it.

#### Scenario: Delete a type in use
- **WHEN** a client deletes environment type `staging` while environment `shop/staging` has that type
- **THEN** the server returns `409` and the type remains

### Requirement: Environment creation

An environment SHALL be created with an `id`, a display `name`, a `type` naming an existing environment type, and optionally `from_deploy_id`. Without `from_deploy_id` the environment is empty. With it, the orchestrator SHALL create a draft delta that reproduces the source deployment's set without deploying it.

#### Scenario: Empty environment
- **WHEN** a client creates environment `staging` of type `staging` in `shop` without a source
- **THEN** the environment exists, has no deployments and no active resources

#### Scenario: Clone from a deployment
- **WHEN** a client creates environment `pr-42` with `from_deploy_id` set to a deployment of `shop/development`
- **THEN** the environment exists with one draft delta whose application to the empty set yields the source deployment's set, and nothing is deployed

### Requirement: Environment deletion deprovisions resources

Environment deletion SHALL use the deployment mutation guard, expected generation, idempotency key and `deployment-safety` rules. Owned resources, including retained dependencies, SHALL be deleted in reverse retained dependency order before the environment. Failure SHALL preserve the environment and remaining records for retry. The default `development` environment SHALL not be deleted while its application exists.

#### Scenario: Successful deletion
- **WHEN** an authorized client deletes `shop/pr-42` with a matching permit for its owned workload, postgres and namespace
- **THEN** the workload, postgres and namespace are deprovisioned in that order and the environment no longer exists

#### Scenario: Deprovision failure
- **WHEN** deprovisioning the postgres fails
- **THEN** the environment still exists, lists the failed resource with its error, and the deletion can be retried

### Requirement: Environment runtime summary

`GET /orgs/{org}/apps/{app}/envs/{env}` SHALL include `generation`, `desired_set_id`, `last_successful_set_id`, `last_successful_deploy_id`, the most recent attempt and its status, and convergence (`converged`, `partial`, `unknown` or `never_deployed`). It SHALL NOT describe a partially applied set as wholly deployed.

#### Scenario: After a deployment
- **WHEN** a deployment to `shop/development` succeeds
- **THEN** reading the environment shows that deployment's ID, status `succeeded`, equal desired and last-successful set IDs, and convergence `converged`
