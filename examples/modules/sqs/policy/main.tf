# The `aws-policy` an `sqs` definition co-provisions: a managed policy that
# sends to and consumes one queue, which is also what a Lambda event source
# mapping needs.

terraform {
  required_version = ">= 1.8"
  required_providers {
    aws = { source = "hashicorp/aws", version = "~> 6.0" }
  }
}

variable "name" {
  type        = string
  description = "The policy name, unique in the account."
}

variable "queue_arn" {
  type        = string
  description = "The queue's ARN, such as `$${resources.sqs.outputs.arn}`."

  validation {
    condition     = can(regex("^arn:aws[a-z-]*:sqs:[a-z0-9-]+:[0-9]{12}:[A-Za-z0-9_.-]+$", var.queue_arn))
    error_message = "`queue_arn` must be an SQS queue ARN."
  }
}

variable "tags" {
  type    = map(string)
  default = {}
}

data "aws_iam_policy_document" "this" {
  statement {
    sid = "Queue"
    actions = [
      "sqs:ChangeMessageVisibility",
      "sqs:DeleteMessage",
      "sqs:GetQueueAttributes",
      "sqs:GetQueueUrl",
      "sqs:ReceiveMessage",
      "sqs:SendMessage",
    ]
    resources = [var.queue_arn]
  }
}

resource "aws_iam_policy" "this" {
  name        = var.name
  description = "Access to the SQS queue ${var.queue_arn}"
  policy      = data.aws_iam_policy_document.this.json
  tags        = var.tags
}

output "arn" {
  value = aws_iam_policy.this.arn
}

output "document" {
  value = data.aws_iam_policy_document.this.json
}
