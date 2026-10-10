# Proposal

## Why

openporch exists so that a single engineer can run their own internal developer platform without a SaaS orchestrator: developers describe a workload once with Score, platform engineers describe with OpenTofu how each kind of resource is provisioned, and the orchestrator turns the two into running infrastructure and workloads per environment. Nothing exists in the repository yet, so this first change establishes the behavior contract for the core orchestrator before any Rust is written.

## What Changes

This change specifies milestone 1 of openporch: a self-hostable orchestrator that can take a Score workload and deploy it, together with its AWS resources, into a Kubernetes environment. It introduces:

- A **control plane API** (HTTP/JSON, single organization) with bearer-token authentication, which every other surface (CLI, OpenTofu provider) talks to.
- **Applications, environment types and environments**, including cloning an environment from an existing deployment and deprovisioning on environment deletion.
- A **resource catalog**: a built-in set of resource types (initially `workload`, `k8s-cluster`, `k8s-namespace`, `base-env`, `config`, `dns`, `postgres`, `postgres-instance`, `mysql`, `redis`, `s3`, `sqs`, `sns-topic`, `dynamodb-table`, `aws-role`, `aws-policy`, `k8s-service-account`, `route`), resource classes, and custom resource types.
- **Resource definitions** with driver inputs, matching criteria with Humanitec-compatible specificity ordering, placeholders, and co-provisioning.
- **Cloud accounts** for AWS (static keys and role assumption) whose temporary credentials are injected into drivers.
- **Score ingestion**: a `score.yaml` (plus optional openporch extension file) becomes a workload in a deployment delta.
- **Deployment sets and deltas**: immutable, content-addressed sets; mutable deltas applied to the environment's current set.
- **Deployments** with full history, per-resource status, retries, failure semantics, redeploy/rollback and set diffing.
- A **resource graph** engine: dependency discovery from Score requests, resource references and co-provisioning; topological ordering; parallel provisioning.
- A **driver model** with in-process drivers (`echo`, `template`) and an **OpenTofu driver** that runs `tofu` against a Git source or inline script with credentials from a cloud account, plus a Humanitec-compatible external HTTP driver protocol.
- **Kubernetes workload deployment**: the implicit `workload` resource is rendered into Deployment/Service/ConfigMap/Secret manifests and applied to the cluster selected by the `k8s-cluster` and `k8s-namespace` resources (static kubeconfig or EKS via a cloud account).
- **Active resources** with stable GUResIDs, scheduled deletion and deprovisioning rules.
- A **CLI** (`porch`) mirroring `humctl` for the day-to-day developer loop.
- An **OpenTofu provider** (`openporch/openporch`) exposing the platform-engineering objects (applications, environment types, resource types, classes, definitions, criteria, cloud accounts, shared values).

### Out of scope for milestone 1

- Multiple organizations, RBAC roles, users and OIDC login (one org, static API tokens only).
- Resource definition versions, resource selectors (`<`, `>` graph queries), incremental deployment mode, drift detection.
- Pipelines, webhooks, artefact automation, a web portal, GitOps mode, a separate in-cluster operator and agent (openporch talks to clusters directly).
- Secret stores other than openporch's own encrypted store; secret references to external stores.
- Workload profiles beyond one built-in profile; serverless/non-Kubernetes workload targets; container logs.
- Non-AWS clusters and drivers (GCP, Azure); database drivers that create databases over a direct connection.

## Capabilities

### New Capabilities

- `api-access`: HTTP/JSON control plane shape, single organization, bearer-token authentication, error format.
- `applications-and-environments`: applications, environment types, environments, cloning and deletion.
- `resource-catalog`: built-in and custom resource types with input/output schemas, and resource classes.
- `resource-definitions`: definition structure, driver inputs, matching criteria and specificity, co-provisioning.
- `cloud-accounts`: AWS credential types and how credentials reach drivers.
- `score-workloads`: supported Score subset, extension file, translation into a workload and resource dependencies.
- `deployment-sets-and-deltas`: immutable sets, mutable deltas, applying deltas, diffing.
- `deployments`: triggering, modes, lifecycle, status, retries, failure semantics, redeploy and rollback.
- `resource-graph`: graph construction, dependency sources, ordering, parallel provisioning, context of a resource.
- `placeholders`: placeholder syntax, context and resource placeholders, type and secret propagation rules.
- `drivers`: driver contract, built-in echo and template drivers, external HTTP drivers.
- `opentofu-driver`: provisioning resources by running OpenTofu with Git or inline sources, state handling, outputs, destroy.
- `kubernetes-workloads`: how a workload is rendered into Kubernetes manifests and applied to the matched cluster and namespace.
- `active-resources`: identity, listing, scheduled deletion, deprovisioning and detaching.
- `cli`: the `porch` command line surface for developers and platform engineers.
- `opentofu-provider`: the OpenTofu provider resources that manage platform objects.

### Modified Capabilities

None. The repository has no existing specs.

## Impact

- Creates the first `openspec/specs/*` capabilities; every later change will build on them.
- Decides the public surfaces of the project: the REST API paths, the CLI, the provider schema, the driver HTTP protocol and the supported Score subset. These are intended to stay close to Humanitec's so that existing Score files, resource definition YAML and mental models carry over.
- Introduces external dependencies at implementation time: the `tofu` binary at runtime, a Kubernetes cluster and AWS account for integration tests, and the OpenTofu plugin protocol for the provider.
