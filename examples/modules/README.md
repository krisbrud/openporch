# Reference OpenTofu modules

Modules for the serverless dependencies of the AWS reference platform, run by
the [`openporch/opentofu` driver](../../docs/drivers/opentofu.md). Each resource
module returns the outputs its resource type requires. Next to it, a `policy/`
module creates the least-privilege managed policy that the definition
co-provisions as an `aws-policy` resource, whose `arn` output a workload's
execution role attaches.

| Module | Type | Outputs | Params (`var.resource`) |
| --- | --- | --- | --- |
| `dynamodb-table` | `dynamodb-table` | `name`, `arn`, `region`, `stream_arn` with a stream | `primaryKey`, `sortKey`, `attributes`, `indexes`, `stream`, `pointInTimeRecovery` |
| `dynamodb-table/policy` | `aws-policy` | `arn` | |
| `sqs` | `sqs` | `arn`, `url`, `region` | `fifo`, `visibilityTimeout`, `messageRetention`, `maxReceiveCount` |
| `sqs/policy` | `aws-policy` | `arn` | |
| `s3` | `s3` | `bucket`, `region`, `arn` | `versioning`, `expireAfterDays` |
| `s3/policy` | `aws-policy` | `arn` | |
| `aws-lambda-workload` | `workload` | `function_name`, `function_arn`, `role_arn`, `region`, `url` with an `http` trigger | the workload document |

Each module's header describes its params. The resource modules take the
resource's name as the variable `name`; the policy modules take `name` and the
resource's ARN (`table_arn`, `queue_arn` or `bucket_arn`), and `read_only` for
tables and buckets or `access` (`send`, `consume` or `send-and-consume`) for
queues. The table policy also reads the table's stream. Every module also takes `tags`. The modules use the
`hashicorp/aws` provider 6.x (6.28 or later for `aws-lambda-workload`) with the credentials the driver exports for the
definition's cloud account. Deleting an `s3` resource deletes its objects too,
so, like every deletion the driver plans, it needs an acknowledgement.

A `dynamodb-table` definition that uses the module from this repository, and
co-provisions its policy:

```json
{
  "id": "dynamodb-table",
  "type": "dynamodb-table",
  "driver_type": "openporch/opentofu",
  "driver_account": "aws-prod",
  "driver_inputs": {
    "values": {
      "source": {"url": "https://github.com/krisbrud/openporch", "rev": "main", "path": "examples/modules/dynamodb-table"},
      "variables": {"name": "${context.app.id}-${context.env.id}-${context.res.id}"}
    }
  },
  "provision": {"aws-policy": {"match_dependents": true}}
}
```

```json
{
  "id": "dynamodb-table-policy",
  "type": "aws-policy",
  "driver_type": "openporch/opentofu",
  "driver_account": "aws-prod",
  "driver_inputs": {
    "values": {
      "source": {"url": "https://github.com/krisbrud/openporch", "rev": "main", "path": "examples/modules/dynamodb-table/policy"},
      "variables": {
        "name": "${context.app.id}-${context.env.id}-${context.res.id}",
        "table_arn": "${resources.dynamodb-table.outputs.arn}"
      }
    }
  }
}
```

Pin `rev` to a tag or commit in a real platform. Names built from a resource ID
such as `modules.api.externals.orders` contain dots, which tables accept but
queues and buckets do not; `examples/aws-platform` derives valid names.

## Lambda workloads

`aws-lambda-workload` runs a `workload` resource of class `aws-lambda` as one
Lambda function named `<app>-<env>-<workload>`; its header describes what it
reads from the workload document. The definition passes the application and
environment IDs:

```json
{
  "id": "lambda",
  "type": "workload",
  "driver_type": "openporch/opentofu",
  "driver_account": "aws-prod",
  "driver_inputs": {
    "values": {
      "source": {"url": "https://github.com/krisbrud/openporch", "rev": "main", "path": "examples/modules/aws-lambda-workload"},
      "variables": {"app": "${context.app.id}", "env": "${context.env.id}"}
    }
  },
  "criteria": [{"class": "aws-lambda"}]
}
```

Secret variables never reach the function's environment. Each is a SecureString
parameter `/openporch/<function>/<NAME>` (or, with
`target.lambda.secrets: secretsmanager`, a secret
`openporch/<function>/<NAME>`) that only the function's role may read, and the
function finds its name in `<NAME>_SECRET_REF` and reads it at startup. A
secret anywhere else in the workload, such as in a container's `args` or a
schedule's `input`, is refused rather than dropped. The values are kept in the
OpenTofu state, like every secret a module writes, so protect the state.

A container workload runs an image that must answer Lambda's runtime API. For
an ordinary HTTP server, add the [Lambda Web
Adapter](https://github.com/awslabs/aws-lambda-web-adapter) to the image,
pinned to a release, and the module tells it the service's port through
`AWS_LWA_PORT`:

```dockerfile
COPY --from=public.ecr.aws/awsguru/aws-lambda-adapter:0.9.1 /lambda-adapter /opt/extensions/lambda-adapter
```

Setting `AWS_LWA_PORT` does not by itself make an image run on Lambda.

Schedules translate 5-field cron expressions into EventBridge Scheduler's
`cron(...)`, which numbers days of the week from Sunday = 1 and cannot restrict
both the day of the month and the day of the week; such an expression, a step
in the day of the week, a value out of its field's range, a reversed range,
macros such as `@daily`, and a time zone that is not an IANA name or `UTC` are
refused rather than approximated. The checks run when OpenTofu plans the workload, so they stop
the function from being created but come after its dependencies are
provisioned.

## Testing

`test.sh` runs `tofu fmt -check`, then `tofu validate` and `tofu test` in every
module. The tests in each `tests/` directory plan and apply the module against
the AWS account or emulator the environment points at and destroy what they
created, except those of `aws-lambda-workload`, which plan against a mocked
AWS provider. CI runs them against [Floci](https://floci.io):

```sh
AWS_ENDPOINT_URL=http://localhost:4566 AWS_ACCESS_KEY_ID=test \
  AWS_SECRET_ACCESS_KEY=test AWS_REGION=us-east-1 examples/modules/test.sh
```

Against an emulator, S3 Control calls go to `000000000000.localhost`, which must
resolve to the emulator, for example through `/etc/hosts`.
