# Serverless platform on AWS

This OpenTofu configuration sets up an openporch organization to run workloads
on AWS Lambda with DynamoDB, SQS and S3, without Kubernetes. Everything it
creates is pay-per-use, apart from the artifact versions it keeps; see
[Cost](#cost).

It creates, with the [openporch provider](../../provider) and the AWS provider:

- In AWS:
  - the role openporch assumes, `<prefix>-platform`, which only
    `trusted_principal_arn` may assume, with the external ID;
  - `<prefix>-boundary`, the permissions boundary of every role openporch
    creates;
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

Tables are named `<app>.<env>.<resource ID>`, such as
`shop.development.modules.order-worker.externals.orders`. Queue names take no
dots, bucket names are unique across AWS, and both, like policy names, have
length limits, so queues, buckets and policies are `<prefix>-<guresid>`, from
the resource's permanent ID. Everything is tagged with `openporch:app`,
`openporch:env` and `openporch:resource`.

The dependency definitions match only the `default` class, and their policies
refer to `resources.<type>.default`; a definition for another class needs a
policy definition that refers to that class.

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

Add `-var modules_rev=<tag or commit>` to pin the modules; the default, `main`,
changes under you. `trusted_principal_arn` is the identity the openporch server runs as. Keep the
external ID; changing it updates the role and the cloud account together. Then
check that openporch can assume the role:

```sh
curl -X POST -H "Authorization: Bearer $OPENPORCH_TOKEN" \
  "$OPENPORCH_URL/orgs/$OPENPORCH_ORG/resources/accounts/aws/check"
```

Give the platform an AWS account of its own. The role may change every Lambda
function, DynamoDB table, SQS queue and function log group in the account, as
well as schedules, secret parameters under `/openporch/`, buckets named
`<prefix>-<guresid>`, and the repository policies of ECR repositories, and
read the artifact bucket and ECR images.

In IAM it is fenced in. It creates roles and policies only under the path
`/openporch/`, each role it creates must carry the `<prefix>-boundary`
permissions boundary, and it may attach only policies under that path. The
boundary allows data access (DynamoDB, SQS, S3, reading secret parameters,
writing logs, invoking functions) and nothing in IAM. So a function can be
given access to data, but neither openporch nor a function it creates can
change roles or policies it did not create, including its own role and the
boundary.

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

Every version is kept, also after the key is overwritten or the object
deleted, so every deployment can be rolled back to and replayed. Setting
`artifact_noncurrent_days` deletes a version that many days after it stops
being current, which saves storage but makes the deployments that ran it fail
on replay, before they change anything. Incomplete uploads are removed after 7
days.

## Cost

Lambda, DynamoDB on-demand, SQS, EventBridge Scheduler and SSM standard
parameters cost nothing while idle beyond small per-request and storage
charges, and the free tiers often cover a hobby project's traffic. What keeps
costing:

- artifact versions in the bucket, all of which are kept unless
  `artifact_noncurrent_days` is set;
- data in tables and buckets, which deleting an environment removes only with
  a permit;
- CloudWatch logs, kept for 14 days by the Lambda module;
- Secrets Manager secrets, if a workload asks for them instead of SSM
  parameters;
- provisioned concurrency, if a workload sets it;
- the openporch server itself, and the OpenTofu state if kept in S3.

**Delete every environment before destroying this configuration.** It does not
remove what deployments created, and while environments still have resources
the server refuses to delete the definitions, after the criteria are already
gone. Destroying removes the definitions, the cloud account, the role, the
boundary and the artifact bucket, which must first be emptied of every object
version and delete marker.
