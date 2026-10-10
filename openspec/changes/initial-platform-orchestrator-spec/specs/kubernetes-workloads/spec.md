# Spec Delta

## Purpose

Workloads whose `workload` definition uses the built-in Kubernetes driver are rendered into manifests and applied to the cluster and namespace that definitions select for the environment.

## Delivery scope

This capability is milestone 2. Its contracts are retained here for design review; Kubernetes is not a first-release prerequisite.

## ADDED Requirements

### Requirement: Kubernetes workload driver

The built-in driver `openporch/kubernetes` SHALL be usable only for the `workload` type. Its driver inputs SHALL name the cluster and namespace through placeholders on `k8s-cluster` and `k8s-namespace` outputs, which add those resources to the graph.

#### Scenario: Cluster reference
- **WHEN** the `workload` definition for class `kubernetes` sets `cluster: ${resources.k8s-cluster.outputs.name}` and `namespace: ${resources.k8s-namespace.outputs.namespace}`
- **THEN** both resources are provisioned before the workload and the driver applies manifests to that cluster and namespace

### Requirement: Cluster drivers

Built-in drivers SHALL include `openporch/k8s-cluster-static` (inputs `server`, `ca_data`, secrets `token` or `client_cert`/`client_key`) and `openporch/k8s-cluster-eks` (inputs `name`, `region`, `loadbalancer`, with a `driver_account`).

#### Scenario: EKS cluster
- **WHEN** the `k8s-cluster` definition uses `openporch/k8s-cluster-eks` with `driver_account: aws-dev`
- **THEN** the orchestrator obtains a token for the cluster through the account's credentials and outputs `name` and `loadbalancer`

#### Scenario: Namespace per environment
- **WHEN** the `k8s-namespace` definition uses the template driver to output `namespace: ${context.app.id}-${context.env.id}`
- **THEN** the namespace `shop-dev` is created and its UID recorded if missing; an existing namespace requires explicit recorded adoption before workloads are applied

### Requirement: Workload rendering

The Kubernetes driver SHALL render containers into Deployment, optional Service, ConfigMap/Secret files and secret env references, with app/env/workload/deployment labels. Exactly one schedule without service/HTTP renders a CronJob instead. Function, queue/topic/stream, multiple-schedule and mixed scheduled/HTTP workloads SHALL fail preflight. Probes and resource requests SHALL follow the supported container schema.

#### Scenario: Function workload on Kubernetes
- **WHEN** a function workload is matched to an `openporch/kubernetes` definition
- **THEN** preflight fails stating the Kubernetes driver supports container workloads only

#### Scenario: Scheduled container
- **WHEN** a container workload declares `triggers.nightly.schedule.cron: "0 3 * * *"`
- **THEN** a CronJob with that schedule runs the container

#### Scenario: Render a web workload
- **WHEN** workload `web` has a container `main` with image `nginx`, a non-secret variable, a secret variable and port `80 -> 8080`
- **THEN** the manifests are a Deployment with one container whose env references the plain value and the Secret key, and a Service `web` exposing port 80 to target port 8080

### Requirement: Manifests from resources

`manifests` returned by any resource in the graph SHALL be applied with the workload: `location: namespace` into the environment namespace, `cluster` at cluster scope only with administrator policy permitting the resource kind and ownership, and `container`/`volume` merged into the workload's pod template.

#### Scenario: Service account injection
- **WHEN** the `k8s-service-account` resource returns a ServiceAccount manifest with `location: namespace` and a pod spec patch setting `serviceAccountName`
- **THEN** the ServiceAccount is applied and the Deployment's pod uses it

### Requirement: Apply and readiness

Apply SHALL use server-side apply with field manager `openporch` and no forced conflicts. Deployment readiness requires the applied observed generation and all desired replicas updated/available within `deploy.timeout` (default 5m); `deploy.status: deploy` needs only apply. CronJob readiness means its expected specification and enabled schedule are persisted, not that a future Job ran. Runtime status SHALL report last schedule/recent Job outcomes separately.

#### Scenario: Crash-looping container
- **WHEN** the Deployment never becomes available within the timeout
- **THEN** the workload resource fails with the pod's last status message

### Requirement: Workload removal and environment deletion

Permitted workload removal SHALL delete its owned Deployment/CronJob, child Jobs, Service, ConfigMaps and Secrets. Retain a UID inventory and prune obsolete owned objects after kind/name changes. Delete a namespace only when recorded owned UID matches; adopted namespaces default to retain. Objects outside the ownership inventory SHALL block namespace deletion until an explicit administrator decision.

#### Scenario: Remove a workload
- **WHEN** a delta removes workload `payments` and is deployed
- **THEN** the `payments` Deployment and Service no longer exist in the namespace

### Requirement: Runtime status

`GET /orgs/{org}/apps/{app}/envs/{env}/runtime` SHALL report, per workload, desired and available replicas and the pod phases, read live from the cluster.

#### Scenario: Read status
- **WHEN** `web` has 2 of 3 replicas available
- **THEN** the runtime response shows `replicas: 3, available: 2` for `web`

### Requirement: Shared manifest ownership

Manifests emitted by shared resources SHALL be tracked once by the owning resource and reference-counted by consumers. Removing one workload SHALL not delete a shared object still used by another. The driver SHALL preserve pure rendering as a separate step from live apply, readiness and status observation.

#### Scenario: Shared service account
- **WHEN** two workloads consume one recorded ServiceAccount and one workload is removed
- **THEN** the ServiceAccount remains for the other workload

#### Scenario: Field conflict
- **WHEN** another controller owns a field openporch attempts to change
- **THEN** apply reports the conflict without forcing ownership or deleting the other controller's object
