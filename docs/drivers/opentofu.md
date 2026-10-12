# The `openporch/opentofu` driver

The OpenTofu driver provisions a resource by running [OpenTofu](https://opentofu.org)
code: an inline `script`, a module from a Git repository, or both. Every OpenTofu
output becomes a resource output, and outputs marked `sensitive = true` become
secret outputs.

The driver is implemented and tested, including against real OpenTofu, but
deployments do not call drivers until the deployment executor lands, so
definitions using it cannot be deployed yet. The server already checks a
definition's inputs when the definition is written: the input names, the shapes
of `source`, `files`, `variables` and `credentials_config`, the repository URL
and revision, `timeout` and `append_logs_to_error`. Inputs that are placeholders,
and whether `credentials_config` has a `driver_account`, are checked at
deployment.

## Inputs

Plain inputs go under `driver_inputs.values`:

| Input | Contents |
| --- | --- |
| `script` | Inline HCL. Alone, it is the module's `main.tf`. With `source`, it is written as `override.tf` in the module. |
| `source.url` | A Git repository: `https://`, `ssh://` or `user@host:path`. The URL must not contain credentials. |
| `source.rev` | A branch, tag or commit. Default `HEAD`. |
| `source.path` | The module's directory within the repository. Default: the repository root. |
| `source.username` | The user name for `source.password`. Default `git`. |
| `variables` | OpenTofu variables, as a map from name to value. |
| `files` | Extra files written into the module, as a map from relative path to content. |
| `credentials_config` | How the cloud account's credentials reach the module. See [Credentials](#credentials). |
| `append_logs_to_error` | `true` attaches up to 64 KB of the run log to an error. Default `false`. |
| `timeout` | How long one call may run: seconds, or a duration such as `15m`, up to `24h`. Default 15 minutes. |

At least one of `script` and `source` is required.

Secret inputs go under `driver_inputs.secret_refs` (or `driver_inputs.secrets`)
with the same structure:

| Input | Contents |
| --- | --- |
| `variables` | Secret OpenTofu variables. |
| `files` | Secret files, such as a key file a provider reads. |
| `source.password` | A password or token for an `https` repository. |
| `source.ssh_key` | A private key for an `ssh` repository. |
| `tokens` | Tokens for private module and provider registries, as a map from host name to token. |

Placeholders such as `${resources.queue.outputs.arn}` work in any input, like in
any other driver. Placeholders in `source` are checked at deployment, not when the
definition is written.

`files` cannot replace the script (`main.tf`), add override files (`override.tf`,
`*_override.tf`) or write OpenTofu's own `.terraform*` files. A Git module whose
root holds an override file is refused for the same reason: an override could
replace the orchestrator's state backend.

## A DynamoDB table

This definition provisions a `dynamodb-table` with an inline script, in the AWS
account `aws-dev`. First, the account (see [cloud-accounts.md](../cloud-accounts.md)):

```http
POST /orgs/acme/resources/accounts
Authorization: Bearer $ADMIN_TOKEN

{
  "id": "aws-dev",
  "type": "aws",
  "config": {"access_key_id": "AKIAIOSFODNN7EXAMPLE", "region": "eu-north-1"},
  "secrets": {"secret_access_key": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"}
}
```

```http-response
201 Created

{"id": "aws-dev", "type": "aws"}
```

The definition names the table after the application, the environment and the
resource's ID, so each environment gets its own table. Use
`${context.res.guresid}` instead if application and environment IDs could run
together into the same name. The `dynamodb-table` type requires the outputs `name`, `arn` and
`region`:

```hcl
terraform {
  required_providers {
    aws = { source = "hashicorp/aws", version = "~> 6.0" }
  }
}

variable "name" { type = string }

resource "aws_dynamodb_table" "this" {
  name         = var.name
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "pk"
  range_key    = "sk"

  attribute {
    name = "pk"
    type = "S"
  }
  attribute {
    name = "sk"
    type = "S"
  }
}

data "aws_region" "current" {}

output "name" { value = aws_dynamodb_table.this.name }
output "arn" { value = aws_dynamodb_table.this.arn }
output "region" { value = data.aws_region.current.region }
```

In the definition, the script is a JSON string:

```http
POST /orgs/acme/resources/defs
Authorization: Bearer $ADMIN_TOKEN

{
  "id": "dynamodb-on-demand",
  "type": "dynamodb-table",
  "driver_type": "openporch/opentofu",
  "driver_account": "aws-dev",
  "driver_inputs": {
    "values": {
      "script": "terraform {\n  required_providers {\n    aws = { source = \"hashicorp/aws\", version = \"~> 6.0\" }\n  }\n}\n\nvariable \"name\" { type = string }\n\nresource \"aws_dynamodb_table\" \"this\" {\n  name         = var.name\n  billing_mode = \"PAY_PER_REQUEST\"\n  hash_key     = \"pk\"\n  range_key    = \"sk\"\n\n  attribute {\n    name = \"pk\"\n    type = \"S\"\n  }\n  attribute {\n    name = \"sk\"\n    type = \"S\"\n  }\n}\n\ndata \"aws_region\" \"current\" {}\n\noutput \"name\" { value = aws_dynamodb_table.this.name }\noutput \"arn\" { value = aws_dynamodb_table.this.arn }\noutput \"region\" { value = data.aws_region.current.region }\n",
      "variables": {"name": "${context.app.id}-${context.env.id}-${context.res.id}"}
    }
  },
  "criteria": [{}]
}
```

```http-response
201 Created

{
  "id": "dynamodb-on-demand",
  "type": "dynamodb-table",
  "driver_type": "openporch/opentofu",
  "driver_account": "aws-dev",
  "version": 1
}
```

A definition that gives neither `script` nor `source` is refused:

```http
POST /orgs/acme/resources/defs
Authorization: Bearer $ADMIN_TOKEN

{
  "id": "dynamodb-empty",
  "type": "dynamodb-table",
  "driver_type": "openporch/opentofu",
  "driver_inputs": {"values": {"variables": {"name": "orders"}}}
}
```

```http-response
400 Bad Request

{"error": "API-003", "message": "`driver_inputs`: set `source`, `script` or both"}
```

The same module from a Git repository, pinned to a tag, reads its variables the
same way. The token here is a secret the definition did not write, so using it
needs a [secret-use grant](../security.md#secret-use-grants):

```json
{
  "values": {
    "source": {"url": "https://github.com/acme/infra.git", "rev": "v1.4.0", "path": "modules/dynamodb"},
    "variables": {"name": "${context.app.id}-${context.env.id}-${context.res.id}"}
  },
  "secret_refs": {
    "source": {"password": {"store": "openporch", "ref": "github/infra-read-token"}}
  }
}
```

## What a run does

Each call runs in a fresh, owner-only directory that is removed afterwards:

1. The module is fetched at `source.rev`, which is resolved to a commit, or the
   script is written. Then `files`, the variable files and the backend override
   are written.
2. `tofu init`, then `tofu plan` saved to a file. The plan's actions are read
   from `tofu show -json`.
3. If the plan deletes or replaces anything, the call needs a permit for exactly
   this plan (see [Plans and permits](#plans-and-permits)).
4. `tofu apply` applies the saved plan, so nothing else is applied. Then `tofu
   output -json` gives the outputs.

Variables go into variable files, with secret variables in a file of their own.
Credentials and tokens go into owner-only files or the environment, never onto a
command line. Nothing from the server's own environment reaches OpenTofu except
`PATH` and the proxy and CA certificate settings.

Before an apply starts, the driver keeps the script, the dependency lock file and
the resolved commit on the server's disk, under `<root>/state/<guresid>`, whichever
backend holds the state. Deleting the resource later plans against that code,
even if the branch has moved on or the definition has changed. Variables, files
and credentials come from the current definition. The resource's cookie carries
the source commit and the backend.

## Credentials

Without `credentials_config`, and with a `driver_account` of type `aws` or
`aws-role`, the driver exports the account's credentials as `AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN` and `AWS_REGION`. The AWS provider
picks them up with no provider configuration.

`credentials_config` maps the account's credential fields (`AccessKeyId`,
`SecretAccessKey`, `SessionToken` and `Region` for AWS) to the run in three ways:

```json
{
  "credentials_config": {
    "environment": {"AWS_ACCESS_KEY_ID": "AccessKeyId", "AWS_SECRET_ACCESS_KEY": "SecretAccessKey"},
    "variables": {"session_token": "SessionToken"},
    "file": "credentials_file"
  }
}
```

- `environment` sets environment variables. It cannot set `PATH`, `HOME`, `TF_*`
  or the AWS endpoint, CA bundle, profile and instance metadata variables.
- `variables` sets secret OpenTofu variables.
- `file` writes every field to an owner-only JSON file and passes its path in
  the named variable.

`credentials_config` needs a `driver_account`.

Runs never fall back to the server's own cloud identity, because instance metadata
is disabled (`AWS_EC2_METADATA_DISABLED=true`). Module code is trusted, though: it
runs with the account's credentials and can reach the network. If the server runs
on EC2 or ECS, also block the metadata addresses `169.254.169.254` and, on ECS,
`169.254.170.2` for the user openporch runs as.

## State

The module's own `backend` block, local or not, is always overridden with state
that openporch manages:

- **Local** (the default): `<root>/state/<guresid>/terraform.tfstate`, readable
  only by the user openporch runs as. This is for single-host installations. Keep
  the root on an encrypted volume, since state holds every value the module
  handles, secrets included.
- **S3**: each resource is an OpenTofu workspace named by its GUResID, so its
  state is `s3://<bucket>/<prefix>/<guresid>/terraform.tfstate`, encrypted at rest
  and locked with S3 lock files or a DynamoDB table. The backend uses its own
  credentials: static keys configured for the backend, or the server's own
  `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`. Resource accounts never need
  access to the state bucket. The backend's keys can reach the whole bucket, and
  module code can read them during a run, so give them access to the state bucket
  and nothing else.

The server's configuration file picks the backend, along with the binaries and
the driver's root, which defaults to `<data_dir>/opentofu`:

```toml
[opentofu]
tofu = "/usr/local/bin/tofu"           # default: `tofu` on the PATH
git = "git"
# root = "/var/lib/openporch/opentofu"
# ssh_known_hosts = "/etc/openporch/known_hosts"

[opentofu.state]
backend = "s3"                         # or "local", the default
bucket = "acme-openporch-state"
region = "eu-north-1"
key_prefix = "openporch"               # the default
# lock_table = "openporch-locks"       # without it, S3 lock files
# endpoint = "http://localhost:4566"   # S3-compatible; plain http only on loopback, no credentials in the URL
# role_arn = "arn:aws:iam::123456789012:role/openporch-state"
```

The S3 backend authenticates with the server's `AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN`.

Each resource remembers the backend its state is in: the bucket, prefix and
endpoint. If the server is later configured with a different backend, previews
and deployments that touch that resource are refused with `409` `DPL-002`, and
the driver itself refuses it with `TOFU-004`, instead of starting from empty
state. Move the state first.

The same applies when state disappears. A resource whose cookie shows it was
applied, but whose state is missing, is reported as needing recovery instead of
being treated as new, which would create every object a second time. So is a
resource whose first apply started, even one that failed partway before it had
a cookie. To recover, restore the state. Or delete what the resource created,
then create an empty file named `deleted` in the resource's record,
`<root>/state/<GUResID>`, and the next deploy starts afresh. The record of a
started apply lives in that same directory, so back it up along with local
state: if the whole directory is lost before a first apply has a cookie, the
driver cannot tell.

The `backend` input, for state in a backend that the module chooses, is not
supported yet.

## Plans and permits

`plan` returns the actions an apply would take: creates, updates, replacements and
deletions of resources, and changes to outputs. A plan that changes nothing
returns no effects, so the deployment executor can skip the apply once it
lands; `put` itself always plans and applies.

A call whose plan deletes or replaces anything fails with `TOFU-002` unless it
carries a permit for that plan's fingerprint. The fingerprint covers the resource,
whether the call applies or destroys, the inputs, the state it plans against, and
each change with its reasons. It does not cover the values, so it reveals no
secrets. A permit for one resource or one plan never authorizes another, and a
permit goes stale when the inputs or the state change.

## Deleting

Deleting plans a destroy against the retained script, commit and lock file,
checks its permit and applies exactly that plan. State is removed only after the
destroy succeeds, and a remote workspace only once its state is empty. If the
destroy fails, state and the retained record stay, and the delete can be retried.
Deleting again after a successful delete does nothing.

## Timeouts and locks

Within one server process, calls for one resource run one at a time. When a call times out, its commands
are interrupted with `SIGINT`, so that OpenTofu writes state and releases its
lock. Whatever is still running after a grace period, one minute by default, is
killed, including provider processes. The next call for the resource waits
until all of them are gone.

If another process holds the resource's state lock, plans and applies wait for
it (30 seconds by default), then fail as a conflict naming the lock ID. A
conflict means nothing was applied. The driver never breaks a lock. When you are
sure no run is using the state, release it with `tofu force-unlock <id>` from a
directory configured with the same backend: for local state, a `backend
"local"` block whose `path` is `<root>/state/<GUResID>/terraform.tfstate`; for
S3, the same bucket and prefix, with `TF_WORKSPACE` set to the GUResID.

## Errors and logs

| Code | Meaning |
| --- | --- |
| `TOFU-001` | A command failed. The error carries the last 20 lines of its error output, and with `append_logs_to_error` the run log. |
| `TOFU-002` | The plan deletes or replaces something and the call carries no matching permit. |
| `TOFU-003` | The driver inputs are invalid. |
| `TOFU-004` | The resource's state is in a different backend than the one configured. |

A failure after an apply has started is reported as an uncertain outcome rather
than an error, since infrastructure may have changed. Retrying converges on the
same state.

The run log goes to the deployment's logs. Every secret input, credential, token
and sensitive output of four or more characters is replaced with `[REDACTED]`
before a line leaves the driver, in its raw form and in the escaped forms OpenTofu prints. The JSON that
`tofu show -json`, `tofu output -json` and `tofu state pull` print is never
logged, since it holds every value.

## Git sources

Modules are fetched with a shallow fetch of exactly `source.rev`. A deployment replayed from its snapshot fetches `source.rev` again, so a
branch can resolve to a newer commit than the one first applied; name a commit
or tag for replays that apply the same code. `https`
repositories authenticate with `source.username` and `source.password`, which
travel in the environment, not the URL. `ssh` repositories authenticate with
`source.ssh_key`. Host keys are checked against the known hosts configured for
the server, and an `ssh` source fails when none are configured.

Registry tokens in `tokens` become `TF_TOKEN_<host>` variables, so private
registries and module sources such as `app.terraform.io` work as usual.

## Not yet

- The `backend` input.
