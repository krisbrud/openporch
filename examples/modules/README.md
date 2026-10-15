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

Each module's header describes its params. The resource modules take the
resource's name as the variable `name`; the policy modules take `name` and the
resource's ARN (`table_arn`, `queue_arn` or `bucket_arn`), and `read_only` for
tables and buckets. Every module also takes `tags`. The modules use the
`hashicorp/aws` provider 6.x with the credentials the driver exports for the
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

## Testing

`test.sh` runs `tofu fmt -check`, then `tofu validate` and `tofu test` in every
module. The tests in each `tests/` directory plan and apply the module against
the AWS account or emulator the environment points at and destroy what they
created. CI runs them against [Floci](https://floci.io):

```sh
AWS_ENDPOINT_URL=http://localhost:4566 AWS_ACCESS_KEY_ID=test \
  AWS_SECRET_ACCESS_KEY=test AWS_REGION=us-east-1 examples/modules/test.sh
```

Against an emulator, S3 Control calls go to `000000000000.localhost`, which must
resolve to the emulator, for example through `/etc/hosts`.
