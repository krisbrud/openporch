# Spec Delta

## Purpose

Placeholders let definitions and workloads refer to deployment context and to outputs of other resources, resolved by the orchestrator at deployment time.

## ADDED Requirements

### Requirement: Placeholder syntax

A placeholder SHALL be written `${...}`. The literal sequence `$\{` SHALL evaluate to `${`. A string MAY contain several placeholders and surrounding text. Unknown placeholder roots or unresolvable references SHALL fail the deployment with an error naming the placeholder and where it appears.

#### Scenario: Composite string
- **WHEN** a driver input is `${context.app.id}-${context.env.id}` in `shop/dev`
- **THEN** it resolves to `shop-dev`

#### Scenario: Unknown root
- **WHEN** a driver input contains `${secrets.foo}`
- **THEN** the deployment fails naming `${secrets.foo}` and the definition

### Requirement: Context placeholders

In `driver_inputs`, `provision.params` and `driver_account`, the orchestrator SHALL resolve `context.org.id`, `context.app.id`, `context.env.id`, `context.env.type`, `context.res.id`, `context.res.class`, `context.res.type`, `context.res.guresid`, and `context.deploy.id`, `context.deploy.set.id`.

#### Scenario: Namespace from context
- **WHEN** the k8s-namespace definition sets `namespace: ${context.app.id}-${context.env.id}`
- **THEN** the namespace resource is provisioned with the name `shop-dev`

### Requirement: Resource placeholders

`${resources.<type>[.<class>][#<id>].outputs.<path>}` SHALL resolve to the named output of that resource, `${resources....guresid}` to its GUResID, and `${resources....account}` (only in `driver_account`) to its cloud account ID. Bracket form `${resources['dns.public#shared.api-dns'].outputs.host}` SHALL be accepted.

#### Scenario: Cloud account from a config resource
- **WHEN** a definition sets `driver_account: ${resources['config.default#aws-account'].account}`
- **THEN** the driver receives credentials for the cloud account of that config resource

### Requirement: Type and secret propagation

A string that is exactly one placeholder SHALL take the referenced value's JSON type. A string mixing text and placeholders SHALL insert strings raw and other values as JSON. Any string containing a secret output SHALL be treated as a secret and SHALL only be allowed in `driver_inputs.secrets`, workload secret variables, or secret files.

#### Scenario: Whole-value object
- **WHEN** a driver input is `${resources.config.outputs.limits}` and `limits` is an object
- **THEN** the driver receives the object, not a string

#### Scenario: Secret in a value input
- **WHEN** `driver_inputs.values.url` contains `${resources.postgres.outputs.password}`
- **THEN** the deployment fails stating that a secret cannot be used in a non-secret input

### Requirement: Set placeholders for workloads

Inside workload specs, `${externals.<key>.<output>}`, `${shared.<id>.<output>}`, `${values.<key>}` (shared values, environment level overriding application level), `${context.*}` and `${pod.status.podIP}`-style runtime references SHALL be resolved when rendering the workload; the pod references SHALL render as Kubernetes field references.

#### Scenario: Shared DNS host in a variable
- **WHEN** a workload variable is `https://${shared.api-dns.host}/api`
- **THEN** the container env var carries the provisioned host
