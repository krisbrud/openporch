# Spec Delta

## Purpose

Lets hobby and low-cost platforms run Score workloads as AWS Lambda functions with serverless dependencies such as DynamoDB and SQS, with no Kubernetes cluster involved.

## ADDED Requirements

### Requirement: Lambda workload module

openporch SHALL ship a reference OpenTofu module `aws-lambda-workload` and a `workload` definition for class `aws-lambda` that creates, per workload, one Lambda function from the first container's image (an ECR container image), with environment variables from the resolved Score variables, memory and timeout from extension `spec.lambda.memory_mb` and `spec.lambda.timeout_seconds` (defaults 256 and 30), and an execution role.

#### Scenario: Deploy a Lambda workload
- **WHEN** a Score workload `api` with class `aws-lambda` and container image `123.dkr.ecr.eu-north-1.amazonaws.com/api:abc` is deployed to `shop/dev`
- **THEN** a Lambda function named `shop-dev-api` exists with that image, the resolved variables as environment, 256 MB memory and a 30 second timeout

#### Scenario: Non-ECR image
- **WHEN** the container image is not an ECR image URI
- **THEN** provisioning fails with an error stating Lambda container images must come from ECR

### Requirement: Secrets reach the function without plain text

Secret variables SHALL NOT be written as plain Lambda environment variables. The module SHALL store them in AWS Secrets Manager or SSM Parameter Store (SecureString) and expose their ARNs to the function as `<NAME>_SECRET_ARN` variables, granting the execution role read access.

#### Scenario: Database password
- **WHEN** the workload variable `DB_PASSWORD` resolves from a postgres secret output
- **THEN** the function environment has `DB_PASSWORD_SECRET_ARN` and no `DB_PASSWORD`, and the role may read that secret

### Requirement: HTTP exposure

When the Score file declares `service.ports`, the module SHALL create a Lambda function URL (auth `NONE` by default, `AWS_IAM` when `spec.lambda.url_auth: iam`) and publish it as the workload output `url`.

#### Scenario: Function URL
- **WHEN** the workload declares a service port
- **THEN** the workload's outputs include `url` and an HTTP GET to it invokes the function

### Requirement: Permissions from dependencies

The execution role SHALL be granted access to the workload's AWS dependencies by attaching the policies of `aws-policy` resources co-provisioned by their definitions (for example the DynamoDB table and SQS queue policies from the reference platform).

#### Scenario: DynamoDB access
- **WHEN** the workload depends on a `dynamodb-table` whose definition co-provisions an `aws-policy` for that table
- **THEN** the function can read and write that table without any manual IAM change

### Requirement: Serverless dependency definitions

The reference platform SHALL include OpenTofu-driver definitions for `dynamodb-table` (inputs `primaryKey`, `attributes`, indexes; outputs `arn`, `name`, `region`), `sqs`, `sns-topic` and `s3`, each co-provisioning an `aws-policy` granting least-privilege access to that resource, so a Lambda workload's whole stack is pay-per-use.

#### Scenario: Table from Score
- **WHEN** a Score file declares `resources.orders` of type `dynamodb-table` with `params.primaryKey: {name: id, type: S}`
- **THEN** deploying creates a table with that key and the workload variable `${resources.orders.name}` resolves to its name

### Requirement: Teardown

Deleting the environment SHALL remove the function, its URL, role, secrets and every serverless dependency through `tofu destroy`, leaving no billable resource behind.

#### Scenario: Delete environment
- **WHEN** `shop/dev` is deleted
- **THEN** the Lambda function, function URL, role, secrets, table and queue no longer exist
