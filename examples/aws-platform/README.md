# Serverless platform on AWS

This OpenTofu configuration sets up an openporch organization to run workloads
on AWS Lambda with DynamoDB, SQS and S3, without Kubernetes. Everything it
creates is pay-per-use, apart from the artifact versions it keeps; see
[Cost](#cost).

It creates, with the [openporch provider](../../provider) and the AWS provider:

- In AWS:
  - the role openporch assumes, `<prefix>-platform`, which only
    `trusted_principal_arn` may assume, with the external ID;
  - the versioned artifact bucket `<prefix>-artifacts-<account>`.
- In openporch:
  - the `production` environment type (`development` always exists);
  - the `aws-role` cloud account `aws`;
  - the resource classes it uses;
  - these resource definitions, which use the modules in
    [`examples/modules`](../modules) from `modules_source` at `modules_rev`:

| Definition | Type | Matches | Driver |
| --- | --- | --- | --- |
| `base-env` | `base-env` | every environment | `openporch/none` |
| `aws-lambda-workload` | `workload` | class `aws-lambda` | `openporch/opentofu`, [`aws-lambda-workload`](../modules/aws-lambda-workload) |
| `workload-none` | `workload` | class `default` | `openporch/none`: stores the resolved workload, runs nothing |
| `dynamodb-table`, `sqs`, `s3` | the same | class `default` | `openporch/opentofu`, co-provisioning an `aws-policy` of the same class |
| `dynamodb-table-policy`, `sqs-policy`, `s3-policy` | `aws-policy` | class `dynamodb-table`, `sqs`, `s3` | `openporch/opentofu`, the module's `policy/` |
| `dns` | `dns` | every `dns` | `openporch/echo`: `<app>-<env>.<dns_zone>`, without creating a record |

A workload of class `aws-lambda` that declares a table, queue or bucket gets
it, a policy for it, and an execution role with that policy attached. The
policy is created after the resource and before the function.

AWS names come from the application, environment and resource ID where AWS
accepts them: tables are `<app>-<env>-<resource ID>`, such as
`shop-development-modules.order-worker.externals.orders`, and policies the same
with the type appended. Queue and bucket names take no dots and buckets are
unique across AWS, so those are `<prefix>-<guresid>`, from the resource's
permanent ID. Everything is tagged with `openporch:app`, `openporch:env` and
`openporch:resource`.

## Apply it

Run it as an openporch administrator, with credentials for the AWS account:

```sh
export OPENPORCH_URL=https://porch.example.com OPENPORCH_ORG=acme
export OPENPORCH_TOKEN=...   # an administrator's token
tofu init
tofu apply \
  -var trusted_principal_arn=arn:aws:iam::111122223333:role/openporch-server \
  -var external_id="$(openssl rand -hex 24)"
```

`trusted_principal_arn` is the identity the openporch server runs as. Keep the
external ID; changing it updates the role and the cloud account together. Then
check that openporch can assume the role:

```sh
curl -X POST -H "Authorization: Bearer $OPENPORCH_TOKEN" \
  "$OPENPORCH_URL/orgs/$OPENPORCH_ORG/resources/accounts/aws/check"
```

The role may create and delete Lambda functions, their logs, schedules and
secret parameters under `/openporch/`, DynamoDB tables, SQS queues, buckets
named `<prefix>-<guresid>`, IAM roles and policies, and read the artifact
bucket and ECR images. Since it creates execution roles and the policies
attached to them, it can grant those roles anything, though not change its
own; let only openporch assume it.

`examples/aws-platform/test.sh` applies the configuration against a dev server,
checks what the server holds, plans again to see that nothing changes, and
destroys it. CI runs it against [Floci](https://floci.io), an
AWS emulator.

## Function artifacts

CI uploads each build of a zip function to the artifact bucket under a key
that names its content, and deploys it by that key. openporch pins the
deployment to the object's version when it accepts it, so a later upload to the
same key does not change what an earlier deployment, its rollback or its
replay runs:

```sh
bucket=$(tofu output -raw artifact_bucket)
key="order-worker/$(sha256sum build/order-worker.zip | cut -c1-64).zip"
aws s3 cp build/order-worker.zip "s3://$bucket/$key"
porch workload deploy -f workload.yaml --artifact "main=s3://$bucket/$key" --wait
```

A version that stops being current, because the key was overwritten or the
object deleted, is kept for `artifact_noncurrent_days` (365 by default) and then
expired; current versions are never expired. Keep that longer than you may want
to roll back or replay a deployment: a deployment whose artifact version is gone
fails before it changes anything. Incomplete uploads are removed after 7 days.

## Cost

Lambda, DynamoDB on-demand, SQS, EventBridge Scheduler and SSM standard
parameters cost nothing while idle beyond small per-request and storage
charges, and the free tiers often cover a hobby project's traffic. What keeps
costing:

- artifact versions in the bucket, until they expire;
- data in tables and buckets, which deleting an environment removes only with
  a permit;
- CloudWatch logs, kept for 14 days by the Lambda module;
- Secrets Manager secrets, if a workload asks for them instead of SSM
  parameters;
- provisioned concurrency, if a workload sets it;
- the openporch server itself, and the OpenTofu state if kept in S3.

Destroying this configuration removes the definitions, the cloud account, the
role and the artifact bucket; the bucket must be empty first. It does not
remove what deployments created: delete the environments first.
