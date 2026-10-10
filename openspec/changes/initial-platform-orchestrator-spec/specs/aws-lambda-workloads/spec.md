# Spec Delta

## Purpose

Lets hobby and low-cost platforms run openporch workloads as AWS Lambda functions with serverless dependencies such as DynamoDB and SQS, with no Kubernetes cluster requirement. Cloud usage, retained data, artifacts, state and hosting the control plane can still incur costs.

## ADDED Requirements

### Requirement: Lambda workload module

openporch SHALL ship a reference OpenTofu module `aws-lambda-workload` and a `workload` definition for class `aws-lambda` that creates one Lambda function per workload named `<app>-<env>-<workload>`, with memory from `function.memory`, timeout from `function.timeout`, concurrency from `function.concurrency`, environment variables from the resolved variables, and an execution role.

#### Scenario: Zip function
- **WHEN** a function workload with `package.type: zip`, `runtime: python3.12`, `handler: app.handler` is deployed to `shop/dev` with `--artifact main=s3://acme-artifacts/order-worker/sha256-abc.zip`
- **THEN** a function `shop-dev-order-worker` exists with that runtime, handler and code object, 256 MB memory and a 30 second timeout

#### Scenario: Image function
- **WHEN** a function workload with `package.type: image` is deployed with an ECR image URI
- **THEN** a function with that image exists and `command`/`args` are not required

#### Scenario: Container workload on Lambda
- **WHEN** a container workload with exactly one container and an ECR image is deployed with class `aws-lambda`
- **THEN** it is deployed as an image function with the container's `command`/`args` as image config overrides, its `service.ports.targetPort` exported as `AWS_LWA_PORT`, and `files` or a second container cause a preflight error. A normal HTTP container SHALL include a compatible Lambda Web Adapter or Lambda runtime interface integration; the reference example SHALL pin and document the adapter and validate an actual HTTP invocation. Setting `AWS_LWA_PORT` alone SHALL NOT be described as making arbitrary container images Lambda-compatible

#### Scenario: Non-ECR image
- **WHEN** the image is not an ECR image URI in the function's region
- **THEN** provisioning fails with an error stating Lambda container images must come from ECR in the same region

### Requirement: Secrets reach the function without plain text

Secret variables SHALL NOT be written as plain Lambda environment variables. The module SHALL store them as SSM Parameter Store `SecureString` parameters by default (Secrets Manager when `target.lambda.secrets: secretsmanager`), expose their names or ARNs to the function as `<NAME>_SECRET_REF` variables, and grant the execution role read access.

#### Scenario: Database password
- **WHEN** the variable `DB_PASSWORD` resolves from a postgres secret output
- **THEN** the function environment has `DB_PASSWORD_SECRET_REF` and no `DB_PASSWORD`, and the role may read that parameter

### Requirement: Triggers

The module SHALL support `http` (a Lambda function URL with auth `NONE` or `AWS_IAM`, published as output `url`), `schedule` (an EventBridge Scheduler schedule translated from 5-field cron and timezone, with `input` as the event payload), and `queue` (an SQS event source mapping with `batchSize`). `topic` and `stream` triggers SHALL fail preflight with an error naming the trigger kind as unsupported in this milestone.

#### Scenario: Function URL
- **WHEN** the workload explicitly declares an `http` trigger with `auth: none`
- **THEN** the workload's outputs include `url` and an HTTP GET to it invokes the function

#### Scenario: Queue trigger
- **WHEN** the workload declares `triggers.inbox.queue` on its `sqs` dependency `inbox`
- **THEN** an event source mapping from that queue to the function exists and the role may receive and delete its messages

#### Scenario: Nightly schedule
- **WHEN** the workload declares `schedule.cron: "0 3 * * *"` with `timezone: Europe/Oslo`
- **THEN** a schedule invokes the function at 03:00 Oslo time

### Requirement: Permissions from dependencies

The execution role SHALL be granted access to the workload's AWS dependencies by attaching the policies of `aws-policy` resources co-provisioned by their definitions (for example the DynamoDB table and SQS queue policies from the reference platform).

#### Scenario: DynamoDB access
- **WHEN** the workload depends on a `dynamodb-table` whose definition co-provisions an `aws-policy` for that table
- **THEN** the function can read and write that table without any manual IAM change

### Requirement: Serverless dependency definitions

The milestone 1 reference platform SHALL include OpenTofu-driver definitions for `dynamodb-table` (inputs `primaryKey`, `attributes`, indexes; outputs `arn`, `name`, `region`), `sqs` and `s3`; milestone 2 adds `sns-topic`, each co-provisioning an `aws-policy` granting least-privilege access to that resource, so a function workload's whole stack is pay-per-use.

#### Scenario: Table from a workload file
- **WHEN** a workload declares `resources.orders` of type `dynamodb-table` with `params.primaryKey: {name: id, type: S}`
- **THEN** deploying creates a table with that key and the variable `${resources.orders.name}` resolves to its name

### Requirement: Artifact bucket

The reference platform SHALL create a versioned artifact bucket with a documented retention policy that preserves artifact versions referenced by retained deployment snapshots, and document how CI uploads a zip under a content-addressed key and deploys it with `--artifact`. The cloud account used by the workload definition SHALL be able to read it.

#### Scenario: Rollback to a previous artifact
- **WHEN** a deployment is rolled back with `+1`
- **THEN** the previous artifact object still exists and the function is updated to it

### Requirement: Teardown

Deleting the environment SHALL remove the function, its URL, schedules, event source mappings, role, parameters and every serverless dependency through permitted OpenTofu destruction of owned resources. Shared artifact/state buckets and externally supplied or explicitly retained resources SHALL be excluded and listed with their ownership and possible continuing cost; teardown SHALL NOT claim zero billing without accounting for retained objects.

#### Scenario: Delete environment
- **WHEN** `shop/dev` is deleted with a matching permit
- **THEN** the function, its triggers, role, parameters, table and queue no longer exist

### Requirement: Runtime secret resolution and target limits

Examples SHALL include tested runtime code/helper fetching `_SECRET_REF` values with the workload identity and document refresh behavior; plaintext variables are not automatically populated. Preflight SHALL validate AWS size/time/region/ECR limits, cron translation and trigger combinations. HTTP URLs SHALL require explicit `auth: none|iam`; an inferred trigger SHALL not silently create a public URL. Unsupported cron expressions SHALL fail, not be approximated.

#### Scenario: Function uses a secret
- **WHEN** the example function is deployed with a database secret reference
- **THEN** an integration invocation fetches that secret with its own scoped role while neither logs nor Lambda environment settings contain its plaintext

#### Scenario: Workload removed
- **WHEN** a permitted deployment removes a Lambda workload while retaining its queue dependency
- **THEN** the function and its owned schedules, URL and event mappings are removed, while the retained queue remains tracked
