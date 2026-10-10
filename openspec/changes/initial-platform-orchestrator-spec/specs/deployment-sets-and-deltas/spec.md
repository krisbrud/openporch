# Spec Delta

## Purpose

A deployment set is the immutable, environment-agnostic description of what runs in an environment; a delta is the mutable description of a change to a set.

## ADDED Requirements

### Requirement: Deployment set content and identity

A set SHALL contain `modules` (workloads keyed by ID, each with `class`, `spec` and `externals`) and `shared` (shared resources keyed by ID with `type`, `class`, `params`). Its ID SHALL be a hash of its canonicalized JSON content so identical sets have identical IDs. Sets SHALL be immutable and readable at `/orgs/{org}/apps/{app}/sets/{set}`.

#### Scenario: Identical content, identical ID
- **WHEN** two deltas applied in different environments produce sets with identical content
- **THEN** both deployments reference the same set ID

### Requirement: Delta structure

A delta SHALL belong to an application and contain `modules.add` (map of workload ID to workload), `modules.remove` (list of IDs), `modules.update` (map of workload ID to JSON Patch operations), and `shared` (list of JSON Patch operations on the shared map). Deltas are mutable until deployed, after which they SHALL be frozen.

#### Scenario: Patch a variable
- **WHEN** a delta updates `products` with `{op: replace, path: /spec/containers/web/variables/PAGE_SIZE, value: "20"}`
- **THEN** applying it to a set where PAGE_SIZE was 10 yields a set where it is 20

#### Scenario: Modify a deployed delta
- **WHEN** a client updates a delta that has already been deployed
- **THEN** the server returns `409`

### Requirement: Applying a delta

Applying a delta to a set SHALL process `modules.remove`, then `modules.add`, then `modules.update`, then `shared`, and yield a new set. Removing or updating a workload that does not exist, or adding one that does, SHALL fail with `400` naming the workload.

#### Scenario: Add to an empty environment
- **WHEN** a delta adding workload `web` is applied to an environment with no deployments
- **THEN** the resulting set contains only `web`

### Requirement: Delta from Score

`POST /orgs/{org}/apps/{app}/deltas` SHALL accept either a raw delta or a Score conversion request, and `porch score deploy` SHALL build a delta whose `modules.add` or `modules.update` brings the named workload to the Score file's content relative to the environment's current set.

#### Scenario: Redeploy unchanged Score
- **WHEN** the same Score file is deployed twice to the same environment
- **THEN** the second delta applied yields the same set ID as the first deployment

### Requirement: Set diff

`GET /orgs/{org}/apps/{app}/sets/{a}/diff/{b}` SHALL return a delta that transforms set `a` into set `b`. The CLI SHALL accept deployment references `deploy/.` (current) and `deploy/+n` (n deployments ago) in place of set IDs.

#### Scenario: Diff last two deployments
- **WHEN** `porch diff sets --app shop --env dev deploy/. deploy/+1` runs
- **THEN** it prints the delta between the current and the previous deployment's sets
