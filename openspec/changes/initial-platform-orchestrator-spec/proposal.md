# Proposal

## Why

openporch exists so that a single engineer can run their own internal developer platform without a SaaS orchestrator: developers describe a workload once in a small, Score-like document that can be containers or a function, platform engineers describe with OpenTofu how each kind of resource is provisioned, and the orchestrator turns the two into running infrastructure and workloads per environment. Nothing exists in the repository yet, so this first change establishes the behavior contract for the core orchestrator before any Rust is written.

## What Changes

This change specifies a staged roadmap for openporch, with milestone 1 focused on a self-hostable orchestrator that can take a workload document and deploy it, together with its AWS resources, as Lambda functions; Kubernetes follows in milestone 2. It introduces the following contracts, with release gates below:

- A **control plane API** (HTTP/JSON, single organization) which every other surface (CLI, OpenTofu provider) talks to.
- **Security built in from the first route**: principals (users and service users), API tokens and 24-hour sessions, pluggable identity providers with GitHub login, invitations and organization membership, Humanitec-compatible RBAC at organization, application and environment type level enforced on every route, and an audit log, privilege-delegation ceilings and secret-use grants.
- **Secret stores and secret references**: a built-in encrypted store plus AWS Secrets Manager and SSM stores, references usable in definitions, cloud accounts and shared values, and secrets written through the orchestrator into one primary store; no API ever returns a secret value.
- An **OIDC provider** so that cloud accounts of type `aws-identity` can assume roles with short-lived tokens instead of standing credentials, deferred beyond the first release; host AWS identity and role assumption suffice initially.
- **Applications, environment types and environments**, including cloning an environment from an existing deployment and deprovisioning on environment deletion.
- A **resource catalog**: a built-in set of resource types (initially `workload`, `k8s-cluster`, `k8s-namespace`, `base-env`, `config`, `dns`, `postgres`, `postgres-instance`, `mysql`, `redis`, `s3`, `sqs`, `sns-topic`, `dynamodb-table`, `aws-role`, `aws-policy`, `k8s-service-account`, `route`), resource classes, and custom resource types.
- **Resource definitions** with driver inputs, matching criteria with Humanitec-compatible specificity ordering, placeholders, and co-provisioning.
- **Cloud accounts** for AWS (static keys and role assumption) whose temporary credentials are injected into drivers.
- An **openporch workload spec** (`openporch.dev/v1`, `kind: Workload`): one document describing either `containers` (Score's container shape) or a `function` (zip or image package, runtime, handler, memory, timeout), plus `triggers` (http, schedule, queue, topic, stream), `resources` and target passthrough. Score files are accepted as an import with an optional overlay, so the project is not tied to containers.
- **Deployment sets and deltas**: immutable, content-addressed sets; mutable deltas applied to the environment's desired set with generation checks.
- **Deployments** with full history, per-resource status, retries, failure semantics, redeploy/rollback and set diffing.
- A **resource graph** engine: dependency discovery from workload resource requests and triggers, resource references and co-provisioning; topological ordering; parallel provisioning.
- A **driver model** with in-process drivers (`echo`, `template`) and an **OpenTofu driver** that runs `tofu` against a Git source or inline script with credentials from a cloud account, plus a Humanitec-compatible external HTTP driver protocol.
- **Pluggable workload targets**: every workload is a `workload` resource matched by class to a definition, so the platform decides where it runs. The roadmap has three targets: milestone 1 ships `openporch/none` (resolve only, no runtime), **AWS Lambda** through a reference OpenTofu module (zip/image functions, explicit function-URL authentication, SSM secret references by default and scoped roles) so hobby platforms can run serverless without a cluster, and milestone 2 adds **Kubernetes** through a built-in driver that renders Deployment/Service/ConfigMap/Secret manifests and applies them to the cluster selected by the `k8s-cluster` and `k8s-namespace` resources (static kubeconfig or EKS via a cloud account).
- **Active resources** with stable GUResIDs, scheduled deletion and deprovisioning rules.
- A **CLI** (`porch`) mirroring `humctl` for the day-to-day developer loop.
- An **OpenTofu provider** (`openporch/openporch`) exposing the platform-engineering objects (applications, environment types, resource types, classes, definitions, criteria, cloud accounts, shared values).

### Delivery gates

| Stage | Required outcome |
| --- | --- |
| Milestone 1: small, safe platform | SQLite server, API/CLI, built-in encrypted secrets with scoped use, tokens and GitHub login, RBAC, durable operation/recovery records, none/echo/template/OpenTofu drivers, the provider resources needed for the AWS reference platform, and a tested Lambda deploy/replay/teardown path. Preview/permits, ownership, protected state, backup/restore and release checks are required. |
| Milestone 2: broader team/platform integrations | Kubernetes, AWS Secrets Manager/SSM store adapters, generic OIDC login, broader provider/type examples and RDS/EKS end-to-end coverage. Existing core security and recovery contracts apply to each added integration. |
| Later | Postgres with tested migration, optional cloud OIDC issuance, remote executors, periodic drift detection, HA, multi-organization tenancy and GitOps delivery. |

Later-stage requirements are retained as design contracts and marked where applicable; they do not block milestone 1. Login revocation, safe retry, ownership and scoped secret access are never deferred hardening. Resource-definition version management can wait because execution snapshots provide the minimal replay/deletion record. A public cloud issuer, external secret service and S3 state backend are not prerequisites for a hobby installation.

### Out of scope for milestone 1

- Multiple organizations, SAML SSO, identity provider groups, email delivery for invitations (invitation links are handed out by the inviter).
- Public resource-definition version management (immutable execution snapshots are required), resource selectors (`<`, `>` graph queries), incremental deployment mode and periodic drift detection.
- Pipelines, webhooks, artefact automation, a web portal, GitOps mode, a separate in-cluster operator and agent (openporch talks to clusters directly).
- External secret-store adapters, Kubernetes, generic OIDC login, Postgres, the cloud OIDC issuer, managed remote execution and broad provider/example coverage. Built-in secret version retention and recovery remain required.
- Workload targets beyond none and AWS Lambda (ECS, Cloud Run, etc.); container logs.
- Non-AWS cloud drivers (GCP, Azure), hostile-plugin/tenant isolation, database drivers over direct connections, a general policy engine and a separate approval service.

## Capabilities

### New Capabilities

- `deployment-safety`: preflight, expiring action permits, ownership, target restrictions and the trusted executor boundary.
- `control-plane-operations`: installation, readiness, single-writer ownership, backup/restore, upgrades, capacity, diagnostics and release compatibility.

- `api-access`: HTTP/JSON control plane shape, single organization, error format, identifiers, health.
- `authentication`: principals, API tokens, sessions and `porch login`, external identity providers (GitHub, OIDC), bootstrap administrator.
- `organization-membership`: organization roles, invitations, service users, role changes and removal.
- `rbac`: role model, permission matrix, deployer gate, enforcement on every route, role management surfaces.
- `audit-log`: recorded events, query, retention.
- `secret-stores-and-references`: secret stores, primary store, reference format, writing through the orchestrator, resolution.
- `shared-values`: application and environment values and secrets, versions, lifecycle.
- `oidc-provider`: issuer, identity tokens for `aws-identity` cloud accounts, setup guidance (sequenced last).
- `applications-and-environments`: applications, environment types, environments, cloning and deletion.
- `resource-catalog`: built-in and custom resource types with input/output schemas, and resource classes.
- `resource-definitions`: definition structure, driver inputs, matching criteria and specificity, co-provisioning.
- `cloud-accounts`: AWS credential types (`aws`, `aws-role`, `aws-identity`) and how credentials reach drivers.
- `workload-spec`: the openporch workload document (containers or function, triggers, resources, deploy, target), Score import, placeholder translation and deploy-time overrides.
- `deployment-sets-and-deltas`: immutable sets, mutable deltas, applying deltas, diffing.
- `deployments`: triggering, modes, lifecycle, status, retries, failure semantics, redeploy and rollback.
- `resource-graph`: graph construction, dependency sources, ordering, parallel provisioning, context of a resource.
- `placeholders`: placeholder syntax, context and resource placeholders, type and secret propagation rules.
- `drivers`: driver contract, built-in echo and template drivers, external HTTP drivers.
- `opentofu-driver`: provisioning resources by running OpenTofu with Git or inline sources, state handling, outputs, destroy.
- `aws-lambda-workloads`: the reference Lambda workload target and serverless dependency definitions (DynamoDB, SQS, SNS, S3) for cluster-free platforms.
- `kubernetes-workloads`: how a workload is rendered into Kubernetes manifests and applied to the matched cluster and namespace.
- `active-resources`: identity, listing, scheduled deletion, deprovisioning and detaching.
- `cli`: the `porch` command line surface for developers and platform engineers.
- `opentofu-provider`: the OpenTofu provider resources that manage platform objects.

### Modified Capabilities

None. The repository has no existing specs.

## Impact

- Creates the first `openspec/specs/*` capabilities; every later change will build on them.
- Keeps the orchestrator core free of any runtime assumption: Kubernetes and Lambda are both just `workload` definitions.
- Decides the public surfaces of the project: the REST API paths, the CLI, the provider schema, the driver HTTP protocol and the workload document. These are intended to stay close to Humanitec's and Score's so that existing Score files, resource definition YAML and mental models carry over.
- Introduces external dependencies at implementation time: the `tofu` binary at runtime, an AWS account for opt-in milestone 1 integration tests, and a Kubernetes cluster only for milestone 2 integration tests, and the OpenTofu plugin protocol for the provider.
