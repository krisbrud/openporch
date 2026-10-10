# Spec Delta

## Purpose

Workloads run on Kubernetes: the implicit workload resource is rendered into manifests and applied to the cluster and namespace that definitions select for the environment.

## ADDED Requirements

### Requirement: Cluster and namespace selection

Every environment's graph SHALL include `k8s-cluster#k8s-cluster` and `k8s-namespace#k8s-namespace`, each resolved through resource definitions like any other type. Built-in drivers SHALL include `openporch/k8s-cluster-static` (inputs `server`, `ca_data`, secrets `token` or `client_cert`/`client_key`) and `openporch/k8s-cluster-eks` (inputs `name`, `region`, `loadbalancer`, with a `driver_account`).

#### Scenario: EKS cluster
- **WHEN** the `k8s-cluster` definition uses `openporch/k8s-cluster-eks` with `driver_account: aws-dev`
- **THEN** the orchestrator obtains a token for the cluster through the account's credentials and outputs `name` and `loadbalancer`

#### Scenario: Namespace per environment
- **WHEN** the `k8s-namespace` definition uses the template driver to output `namespace: ${context.app.id}-${context.env.id}`
- **THEN** the namespace `shop-dev` is created on the cluster if missing and workloads are applied into it

### Requirement: Workload rendering

The `openporch/default` profile SHALL render a workload into a `Deployment` (one container per Score container, env vars from variables, files as a ConfigMap or Secret mounted at their paths, probes and resource requests), a `Service` when `service.ports` is set, and a `Secret` for secret variables. Labels SHALL identify app, env, workload and deployment ID.

#### Scenario: Render a web workload
- **WHEN** workload `web` has a container `main` with image `nginx`, a non-secret variable, a secret variable and port `80 -> 8080`
- **THEN** the manifests are a Deployment with one container whose env references the plain value and the Secret key, and a Service `web` exposing port 80 to target port 8080

### Requirement: Manifests from resources

`manifests` returned by any resource in the graph SHALL be applied with the workload: `location: namespace` into the environment namespace, `cluster` at cluster scope, and `container`/`volume` merged into the workload's pod template.

#### Scenario: Service account injection
- **WHEN** the `k8s-service-account` resource returns a ServiceAccount manifest with `location: namespace` and a pod spec patch setting `serviceAccountName`
- **THEN** the ServiceAccount is applied and the Deployment's pod uses it

### Requirement: Apply and readiness

Manifests SHALL be applied with server-side apply using a field manager `openporch`. A workload counts as `provisioned` when its Deployment reports all replicas available within the extension file's `deploy.timeout` (default 5 minutes); with `deploy.status: deploy` it counts as provisioned once applied.

#### Scenario: Crash-looping container
- **WHEN** the Deployment never becomes available within the timeout
- **THEN** the workload resource fails with the pod's last status message

### Requirement: Workload removal and environment deletion

When a deployment's set no longer contains a workload, its Deployment, Service, ConfigMaps and Secrets SHALL be deleted from the namespace. Deleting an environment SHALL delete the namespace if the `k8s-namespace` resource was created by openporch.

#### Scenario: Remove a workload
- **WHEN** a delta removes workload `payments` and is deployed
- **THEN** the `payments` Deployment and Service no longer exist in the namespace

### Requirement: Runtime status

`GET /orgs/{org}/apps/{app}/envs/{env}/runtime` SHALL report, per workload, desired and available replicas and the pod phases, read live from the cluster.

#### Scenario: Read status
- **WHEN** `web` has 2 of 3 replicas available
- **THEN** the runtime response shows `replicas: 3, available: 2` for `web`
