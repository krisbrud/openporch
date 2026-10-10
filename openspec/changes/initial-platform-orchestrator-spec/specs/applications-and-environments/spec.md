# Spec Delta

## Purpose

Applications group workloads that are deployed together; environments are the places an application instance runs, each of exactly one environment type that resource definitions can match on.

## ADDED Requirements

### Requirement: Application lifecycle

The orchestrator SHALL let clients create, read, list and delete applications under `/orgs/{org}/apps`. Creating an application SHALL also create a default environment `development` of type `development`. Deleting an application SHALL delete all its environments, deprovisioning their resources first.

#### Scenario: Create an application
- **WHEN** a client creates application `shop`
- **THEN** `shop` exists, has one environment `development` with type `development`, and the creating principal is its `owner`

#### Scenario: Delete an application with provisioned resources
- **WHEN** a client deletes an application whose environments have active resources
- **THEN** every active resource of every environment is deprovisioned before the application and its environments are removed

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

Deleting an environment SHALL deprovision every active resource in it in reverse dependency order, then remove the environment. The default `development` environment SHALL NOT be deletable while the application exists. If deprovisioning fails the environment SHALL remain, marked with the failure.

#### Scenario: Successful deletion
- **WHEN** a client deletes `shop/pr-42` which has a workload, a postgres and a namespace
- **THEN** the workload, postgres and namespace are deprovisioned in that order and the environment no longer exists

#### Scenario: Deprovision failure
- **WHEN** deprovisioning the postgres fails
- **THEN** the environment still exists, lists the failed resource with its error, and the deletion can be retried

### Requirement: Environment runtime summary

`GET /orgs/{org}/apps/{app}/envs/{env}` SHALL include the ID of the last deployment, its status, and the set ID currently deployed.

#### Scenario: After a deployment
- **WHEN** a deployment to `shop/development` succeeds
- **THEN** reading the environment shows that deployment's ID, status `succeeded` and its set ID
