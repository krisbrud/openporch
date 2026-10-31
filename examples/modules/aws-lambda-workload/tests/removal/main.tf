# The `Workload removed` scenario against the AWS account or emulator the
# environment points at: a Lambda workload with a queue trigger, a schedule
# and a URL is deployed beside its queue, then removed while the queue stays.
# The queue and the workload are separate resources in openporch; here they
# are one configuration, so that removing the workload leaves the queue in the
# same state. The workload is added only once the queue and the artifact exist,
# as openporch provisions them before it.

terraform {
  required_version = ">= 1.8"
  required_providers {
    aws = { source = "hashicorp/aws", version = ">= 6.28, < 7.0" }
  }
}

variable "workload" {
  type        = bool
  description = "Whether the workload is deployed."
}

# The function's code: a zip with one Python handler, in a bucket of its own.
resource "aws_s3_bucket" "artifacts" {
  bucket        = "openporch-test-worker-artifacts"
  force_destroy = true
}

resource "aws_s3_object" "zip" {
  bucket = aws_s3_bucket.artifacts.bucket
  key    = "worker.zip"
  # app.py: def handler(event, context): return {"statusCode": 200, "body": "ok"}
  content_base64 = "UEsDBBQAAAAAAJNQX12goXLbSgAAAEoAAAAGAAAAYXBwLnB5ZGVmIGhhbmRsZXIoZXZlbnQsIGNvbnRleHQpOgogICAgcmV0dXJuIHsic3RhdHVzQ29kZSI6IDIwMCwgImJvZHkiOiAib2sifQpQSwECFAMUAAAAAACTUF9doKFy20oAAABKAAAABgAAAAAAAAAAAAAAgAEAAAAAYXBwLnB5UEsFBgAAAAABAAEANAAAAG4AAAAAAA=="
}

module "queue" {
  source = "../../../sqs"
  name   = "openporch-test-worker-inbox"
}

module "worker" {
  count  = var.workload ? 1 : 0
  source = "../.."
  app    = "shop"
  env    = "test"
  resource = {
    id    = "worker"
    class = "aws-lambda"
    spec = {
      function = { package = { type = "zip", artifact = "s3://${aws_s3_object.zip.bucket}/${aws_s3_object.zip.key}", runtime = "python3.12", handler = "app.handler" } }
      triggers = {
        inbox   = { queue = { resource = "inbox", batchSize = 5, outputs = { arn = module.queue.arn } } }
        nightly = { schedule = { cron = "0 3 * * *" } }
        web     = { http = { auth = "none" } }
      }
    }
  }
}

output "queue_arn" {
  value = module.queue.arn
}

output "function_name" {
  value = try(module.worker[0].function_name, "")
}

output "url" {
  value = try(module.worker[0].url, "")
}
