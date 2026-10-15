# Getting started

This guide takes you from a clean machine to a Lambda function deployed by
openporch in your own AWS account, without Kubernetes. You will:

1. install the server, the `porch` command line and the OpenTofu provider;
2. run the server with a protected data directory and master key;
3. replace the first, short-lived token with a lasting one;
4. apply the example serverless platform;
5. deploy the [order worker](../examples/lambda-order-worker), a Python function
   with a DynamoDB table, an SQS queue, a nightly schedule and a secret;
6. delete it again.

Everything it creates in AWS is pay-per-use; see [What it costs](#what-it-costs).
It takes about half an hour, most of it waiting on builds and AWS.

The [developer quickstart](quickstart.md) is a shorter tour that needs no cloud
account.

## What you need

- **An AWS account** you can create IAM roles, S3 buckets, Lambda functions,
  DynamoDB tables and SQS queues in. A sandbox account of its own is best: the
  platform's role may change every function, table and queue in the account.
- **AWS credentials** that the AWS CLI can use, from `aws configure`,
  `aws sso login` or environment variables. The openporch server runs with
  them in this guide and assumes the platform's role with them.
- **Tools** on your `PATH`:
  - [rustup](https://rustup.rs), which installs the Rust version this repository
    pins;
  - [Go](https://go.dev/dl/) 1.25 or newer, to build the OpenTofu provider;
  - [OpenTofu](https://opentofu.org/docs/intro/install/) 1.8 or newer, as `tofu`;
  - the [AWS CLI v2](https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html);
  - `git`, `zip`, `openssl` and `jq`.

Check them:

```sh
cargo --version && go version && tofu version && aws --version
git --version && zip -v | head -1 && openssl version && jq --version
aws sts get-caller-identity
```

The last command prints the identity your credentials belong to. Note its
`Account` and `Arn`; you need them below.

## Install openporch

Clone the repository and install the server, `openporch`, and the command
line, `porch`, into `~/.cargo/bin`. Later steps use `$repo` for the clone, so
run them in the same shell, or set it again:

```sh
git clone https://github.com/krisbrud/openporch
cd openporch
repo=$PWD
cargo install --locked --path crates/server
cargo install --locked --path crates/cli
openporch version
```

The server runs OpenTofu modules with the `tofu` and `git` it finds on its
`PATH`.

### The OpenTofu provider

The `openporch/openporch` provider is not in a registry yet, so build it into a
local mirror that OpenTofu installs it from:

```sh
version=0.0.1
platform="$(go env GOOS)_$(go env GOARCH)"
mirror="$HOME/.local/share/openporch/providers"
(cd provider && go build -ldflags "-X main.version=$version" \
  -o "$mirror/registry.opentofu.org/openporch/openporch/$version/$platform/terraform-provider-openporch_v$version" .)
```

Then tell OpenTofu about the mirror in its CLI configuration, `~/.tofurc`. If
you already have that file, merge this `provider_installation` block into it:

```sh
cat >> ~/.tofurc <<EOF
provider_installation {
  filesystem_mirror {
    path    = "$mirror"
    include = ["registry.opentofu.org/openporch/openporch"]
  }
  direct {
    exclude = ["registry.opentofu.org/openporch/openporch"]
  }
}
EOF
```

Every other provider, such as `hashicorp/aws`, still comes from the registry.

## Run the server

### A private data directory

The server keeps everything in one data directory: its SQLite database, the
master key, the first token, and the OpenTofu state of every cloud resource it
creates. State holds every value a module handles, secrets included, so create
the directory readable by you alone:

```sh
install -d -m 700 ~/openporch ~/openporch/data
```

On a laptop, keep it on an encrypted disk (FileVault, LUKS or BitLocker).

### The master key

openporch encrypts the secrets it stores, such as the order worker's signing
key, with a master key. On its first start the server generates one in
`data/master.key`, readable only by you. Whoever has the key and a copy of the
database can read every secret, and without the key the secrets are lost. So
back the key up once the server has started, somewhere other than where you
back up the data directory, such as a password manager.

To provide your own key instead, generate it before the first start and name
it in the configuration with `master_key_file`, or pass it in the
`OPENPORCH_MASTER_KEY` environment variable:

```sh
(umask 077 && openporch keygen > ~/openporch/master.key)
```

Rotating the key is described in [Master keys](security.md#master-keys).

### The configuration

Write the configuration file:

```sh
cat > ~/openporch/openporch.toml <<EOF
org = "acme"
data_dir = "$HOME/openporch/data"
listen = "127.0.0.1:8080"
EOF
```

`org` is your organization's ID. The server listens on the loopback address
only; to serve other machines, put it behind a reverse proxy that terminates
TLS and set `tls_terminated_by_proxy = true`.

### Where cloud resources' state is kept

By default the state of each resource openporch creates is a file under
`data/opentofu/state/`, readable only by you. Back the data directory up, on
the same schedule as the key: if a resource's state is lost, openporch refuses
to touch that resource until the state is restored.

To keep the state in S3 instead, which suits a server that may move to another
host, add a state bucket to the configuration. The bucket needs versioning and
encryption, and access for the server's credentials only:

```toml
[opentofu.state]
backend = "s3"
bucket = "acme-openporch-state"
region = "eu-north-1"
```

Choose before the first deployment: moving state later is a manual step. The
[OpenTofu driver](drivers/opentofu.md#state) describes both backends.

### Start it

Start the server in its own terminal, with the AWS credentials you checked
above. Set `AWS_PROFILE` if they come from a profile:

```sh
openporch serve --config ~/openporch/openporch.toml
```

It logs JSON lines. On the first start it creates organization `acme`, a
bootstrap administrator and a token for it, and logs where it wrote the token.
Check that it is ready from another terminal:

```sh
curl -s http://127.0.0.1:8080/readyz
```

## Log in

### The first token

The bootstrap token is in `~/openporch/data/bootstrap-token`, a file only you
can read. It expires after 24 hours, and it is an administrator, so use it only
to create a lasting credential. Store it in `porch`, which reads it from
standard input and keeps it in an owner-only file next to
`~/.config/openporch/config.toml`:

```sh
export OPENPORCH_URL=http://127.0.0.1:8080 OPENPORCH_ORG=acme
porch login --token < ~/openporch/data/bootstrap-token
porch get apps
```

### A lasting token

Create a service user for yourself with the `administrator` role and a token
for it that expires in 90 days. The token is printed once, so it goes straight
into a file only you can read:

```sh
porch users create-service platform --role administrator --name "Platform administrator"
(umask 077 && porch users tokens create platform --id laptop --expires-in 90d > ~/openporch/platform-token)
porch login --token < ~/openporch/platform-token
```

Then retire the bootstrap token:

```sh
porch users tokens revoke bootstrap bootstrap
rm ~/openporch/data/bootstrap-token
```

To let teammates sign in with GitHub instead of tokens, see
[GitHub sign-in](security.md#github-sign-in).

## Apply the serverless platform

[`examples/aws-platform`](../examples/aws-platform) configures openporch and AWS
for Lambda workloads. In AWS it creates:

- the role openporch assumes, which only the server's identity may assume;
- a permissions boundary for every role openporch creates;
- a versioned bucket for function artifacts.

In openporch it creates the `aws` cloud account and the resource definitions
that turn a workload of class `aws-lambda` into a function, and its table,
queue or bucket into the real thing.

Copy it to a directory of your own, outside the repository, since its state
will hold the external ID and the platform's settings:

```sh
install -d -m 700 ~/openporch/platform
cp "$repo"/examples/aws-platform/*.tf ~/openporch/platform/
cd ~/openporch/platform
```

The role trusts the identity the server runs as. For an IAM user, that is the
`Arn` from `aws sts get-caller-identity`. For a role session, such as AWS SSO or
`assume-role`, whose `Arn` looks like
`arn:aws:sts::111122223333:assumed-role/NAME/SESSION`, it is the role itself.
Its ARN with its path comes from IAM:

```sh
aws iam get-role --role-name NAME --query Role.Arn --output text
```

Pick a region and a name prefix. The prefix names the role and the buckets,
whose names are unique across all of AWS, so make it yours. Pin the modules to
a commit, so that the platform does not change when `main` does:

```sh
cat > terraform.tfvars.json <<EOF
{
  "aws_region": "eu-north-1",
  "trusted_principal_arn": "arn:aws:iam::111122223333:user/you",
  "name_prefix": "porch-you",
  "modules_rev": "$(git -C "$repo" rev-parse HEAD)"
}
EOF
```

Replace the ARN and the prefix. The external ID is a
long random value that the server presents when it assumes the role. Keep it
in a private file, since a later apply needs the same value:

```sh
(umask 077 && openssl rand -hex 24 > external-id)
```

Apply it with your token:

```sh
export OPENPORCH_TOKEN=$(cat ~/openporch/platform-token)
tofu init
tofu apply -var external_id="$(cat external-id)"
```

Then check that openporch can assume the new role. A new role can take a few
seconds to reach STS, so repeat the check if it first fails with `CLD-001`:

```sh
porch api post /orgs/acme/resources/accounts/aws/check
```

It prints the role session's identity. The [platform's README](../examples/aws-platform/README.md)
says what the role may do and how the artifact bucket keeps versions.

The OpenTofu state in this directory holds the external ID. Keep the directory
private, as `install -m 700` made it, or move the state to a backend with
encryption at rest; never commit it.

## Deploy your first function

Go back to the repository and create an application. It starts with a
`development` environment, which these commands use:

```sh
cd "$repo/examples/lambda-order-worker"
porch create app shop --name Shop
export OPENPORCH_APP=shop OPENPORCH_ENV=development
```

The worker checks a signature on each order with a signing key. Create the key
as a secret shared value of the application. openporch writes it to its secret
store, and the function receives only the name of an SSM parameter that holds
it. Keep `KEY` in this shell, since you sign orders with it:

```sh
KEY=$(openssl rand -hex 32)
printf '{"key": "ORDER_SIGNING_KEY", "secret_ref": {"value": "%s"}}' "$KEY" |
  porch api post /orgs/acme/apps/shop/values -d @-
```

Build the function's zip and upload it to the artifact bucket, under a key that
names its content:

```sh
ARTIFACT_BUCKET=$(tofu -chdir="$HOME/openporch/platform" output -raw artifact_bucket)
mkdir -p build && rm -f build/order-worker.zip
(cd app && zip -X -q ../build/order-worker.zip app.py secret_refs.py)
key="order-worker/$(openssl dgst -sha256 -r build/order-worker.zip | cut -c1-64).zip"
aws s3 cp build/order-worker.zip "s3://$ARTIFACT_BUCKET/$key"
```

Deploy [`workload.yaml`](../examples/lambda-order-worker/workload.yaml) with
that artifact:

```sh
porch workload deploy -f workload.yaml --artifact "main=s3://$ARTIFACT_BUCKET/$key" --wait --timeout 30m
```

`--wait` follows the deployment's log until it finishes. openporch resolves the
workload's resources, creates the table, the queue and a policy for each, then
the function with its role, the queue trigger and the nightly schedule. The
first deployment takes a few minutes, since each resource is an OpenTofu run.

See what it created:

```sh
porch resources graph
porch get active-resources
```

### Try it

Send the worker a signed order through its queue, then read it back from the
table:

```sh
queue_url=$(porch get active-resources sqs/modules.order-worker.externals.inbox -o json | jq -r .outputs.url)
table=$(porch get active-resources dynamodb-table/modules.order-worker.externals.orders -o json | jq -r .outputs.name)
body='{"id": "1001", "total": 30}'
signature=$(printf %s "$body" | openssl dgst -sha256 -hmac "$KEY" -r | cut -d' ' -f1)
aws sqs send-message --queue-url "$queue_url" --message-body "$body" \
  --message-attributes "signature={DataType=String,StringValue=$signature}"
sleep 20
aws dynamodb get-item --table-name "$table" --key '{"id": {"S": "1001"}}'
```

The item has `"total": {"N": "30"}`. If it is not there yet, wait a little
and read it again. The function's log is in CloudWatch, under
`/aws/lambda/shop-development-order-worker`.

To ship a change, build and upload a new zip and deploy it the same way. To
go back, `porch deploy deploy +1 --replay snapshot --wait` runs the previous
deployment again, with the exact artifact version it ran. The order worker's
[`ci/deploy.yml`](../examples/lambda-order-worker/ci/deploy.yml) does the upload and
deploy from GitHub Actions.

## Clean up

Deleting the application deletes its environments and everything they created.
The table and the queue hold data, so the deletion needs a permit for each:
preview it, then run the command the preview prints, with its `--plan` and
`--ack` arguments:

```sh
porch delete app shop --preview
```

Once the application is gone, delete the platform. Empty the artifact bucket
first, every version included, then destroy:

```sh
cd ~/openporch/platform
aws s3api list-object-versions --bucket "$ARTIFACT_BUCKET" --output json |
  jq '{Objects: [(.Versions // [])[], (.DeleteMarkers // [])[] | {Key, VersionId}], Quiet: true}' > delete.json
jq -e '.Objects | length > 0' delete.json >/dev/null &&
  aws s3api delete-objects --bucket "$ARTIFACT_BUCKET" --delete file://delete.json
tofu destroy -var external_id="$(cat external-id)"
```

`delete-objects` takes up to 1000 versions a call; repeat it if the bucket has
more. Destroy the platform only after every environment is deleted: it does not
remove what deployments created.

## What it costs

The stack is pay-per-use. Nothing in it is billed by the hour: there are no
instances, NAT gateways, load balancers or Kubernetes clusters, the table uses
on-demand capacity, and the functions have no provisioned concurrency. An idle
platform with an idle application costs a few cents a month:

- Lambda, SQS, EventBridge Scheduler, SSM standard parameters and DynamoDB
  on-demand charge per request, and their free tiers cover a hobby project's
  traffic;
- S3 storage for the artifacts: every version is kept, so that every deployment
  can be replayed, unless you set `artifact_noncurrent_days`;
- DynamoDB storage for what the table holds;
- CloudWatch Logs, kept for 14 days.

The openporch server runs on your machine in this guide, so it costs nothing.
Running it in the cloud, on a small instance or a container, is the largest
fixed cost of a serverless platform. With an S3 state bucket, its storage
costs a fraction of a cent.

Data costs keep running until it is deleted: deleting an environment removes
tables and buckets only with a permit, and the artifact bucket keeps its
versions until the platform is destroyed.

## Next

- [Cloud accounts](cloud-accounts.md): how openporch reaches AWS.
- [Security model](security.md): roles, tokens, secrets and the audit log.
- [The OpenTofu driver](drivers/opentofu.md): writing your own modules and
  definitions.
- [`examples/lambda-api`](../examples/lambda-api): a container-image function
  behind a function URL.
- [The provider's reference](../provider/docs/index.md) and the
  [command reference](cli.md).
