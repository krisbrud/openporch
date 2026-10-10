# Spec Delta

## Purpose

The resource graph is the per-environment directed acyclic graph of every resource a deployment needs, used to pick definitions, order provisioning and flow outputs between resources.

## ADDED Requirements

### Requirement: Graph construction

For a deployment the orchestrator SHALL build a graph with one node per resource descriptor `<type>.<class>#<res_id>`. Nodes come from: one `workload.<class>#modules.<id>` node per workload, each workload's dependencies, the implicit `base-env` node, resource references in matched definitions, and co-provisioned resources. Nothing else is implicit. The graph SHALL be rejected if it contains a cycle.

#### Scenario: Workload with a database
- **WHEN** workload `web` (class `default`, matched to an `openporch/none` definition) requests `postgres` as `db` and the postgres definition references `${resources.postgres-instance.outputs.host}`
- **THEN** the graph is `workload.default#modules.web -> postgres.default#modules.web.externals.db -> postgres-instance.default#modules.web.externals.db`, plus `base-env`

#### Scenario: Kubernetes workload definition adds cluster nodes
- **WHEN** workload `web` is matched to a `workload` definition using `openporch/kubernetes` whose driver inputs reference `${resources.k8s-cluster.outputs.name}` and `${resources.k8s-namespace.outputs.namespace}`
- **THEN** the graph also contains `k8s-cluster.default#k8s-cluster` and `k8s-namespace.default#k8s-namespace`, and the workload depends on both

#### Scenario: Cycle
- **WHEN** definition A references outputs of type B and B's definition references outputs of type A for the same context
- **THEN** the deployment fails with an error listing the cycle

### Requirement: Resource context

Every node SHALL be resolved in a context of org, app, env, env type, resource type, class and resource ID. The same shared resource ID in two environments SHALL be two distinct resources.

#### Scenario: Shared DNS per environment
- **WHEN** `shared.api-dns` is deployed in `dev` and `staging`
- **THEN** each environment has its own active resource with its own outputs

### Requirement: Resource references add dependencies

A placeholder `${resources.<type>[.<class>][#<id>].outputs.<name>}` in a definition's driver inputs or provision params SHALL add a node with that descriptor (inheriting omitted class and ID from the referencing resource) if absent, and an edge from the referencing resource to it.

#### Scenario: Inherit ID
- **WHEN** the `postgres` definition in context `modules.web.externals.db` references `${resources.postgres-instance.outputs.host}`
- **THEN** the added node is `postgres-instance.default#modules.web.externals.db`

### Requirement: Provisioning order and parallelism

Resources SHALL be provisioned in topological order; a resource starts only when all resources it depends on are `provisioned`. Independent resources MAY be provisioned concurrently, bounded by a configurable global driver concurrency limit (default 4) and an optional lower per-environment limit. Across all environments, active OpenTofu subprocesses SHALL count against the same global limit.

#### Scenario: Parallel branches
- **WHEN** a workload depends on `postgres` and `s3` that do not depend on each other
- **THEN** both may be provisioning at the same time and the workload starts after both are provisioned

### Requirement: Output flow

A resource's resolved driver inputs SHALL include the outputs of the resources it references, and the `workload` node's inputs SHALL include the outputs of all its direct dependencies so that set placeholders resolve.

#### Scenario: Database host reaches the workload
- **WHEN** the postgres resource outputs `host: db.internal`
- **THEN** the workload's variable `DB_HOST: ${externals.db.host}` renders as `db.internal`

### Requirement: Graph inspection

`GET /orgs/{org}/apps/{app}/envs/{env}/resources/graph` SHALL return the last deployment's graph with nodes (descriptor, definition ID, status) and edges, and `porch resources graph` SHALL print it.

#### Scenario: Inspect after deployment
- **WHEN** a client reads the graph after a failed deployment
- **THEN** every node shows its status and the failed node shows its error

### Requirement: Deterministic shared-resource inputs

Requests for the same resource descriptor SHALL merge only when their canonical resource inputs agree. The graph SHALL be independent of workload iteration order. Conflicting definitions, parameters or ownership SHALL produce a preflight error identifying both request paths, without provisioning either resource.

#### Scenario: Conflicting shared table
- **WHEN** two workloads name the same shared DynamoDB table with different primary keys
- **THEN** graph construction rejects both request paths and no driver mutates infrastructure
