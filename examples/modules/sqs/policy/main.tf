# The `aws-policy` an `sqs` definition co-provisions: a managed policy that
# sends to or consumes one queue, or both. Consuming is also what a Lambda
# event source mapping needs.

terraform {
  required_version = ">= 1.8"
  required_providers {
    aws = { source = "hashicorp/aws", version = "~> 6.0" }
  }
}

variable "name" {
  type        = string
  description = "The policy name, unique in the account."

  validation {
    condition     = can(regex("^[A-Za-z0-9+=,.@_-]{1,128}$", var.name))
    error_message = "A policy name has 1 to 128 letters, digits and `+=,.@_-`."
  }
}

variable "queue_arn" {
  type        = string
  description = "The queue's ARN, such as `$${resources.sqs.outputs.arn}`."

  validation {
    condition     = can(regex("^arn:aws[a-z-]*:sqs:[a-z0-9-]+:[0-9]{12}:[A-Za-z0-9_.-]+$", var.queue_arn))
    error_message = "`queue_arn` must be an SQS queue ARN."
  }
}

variable "access" {
  type        = string
  default     = "send-and-consume"
  description = "What the policy grants: `send`, `consume` or `send-and-consume`."

  validation {
    condition     = contains(["send", "consume", "send-and-consume"], var.access)
    error_message = "`access` is `send`, `consume` or `send-and-consume`."
  }
}

variable "iam_path" {
  type        = string
  default     = "/"
  description = "The policy's IAM path, such as `/openporch/`, which lets a platform's role manage only what openporch creates."

  validation {
    condition     = can(regex("^/([A-Za-z0-9+=,.@_-]+/)*$", var.iam_path))
    error_message = "An IAM path starts and ends with `/`."
  }
}

variable "tags" {
  type    = map(string)
  default = {}
}

locals {
  send    = ["sqs:GetQueueAttributes", "sqs:GetQueueUrl", "sqs:SendMessage"]
  consume = ["sqs:ChangeMessageVisibility", "sqs:DeleteMessage", "sqs:GetQueueAttributes", "sqs:GetQueueUrl", "sqs:ReceiveMessage"]
  actions = {
    send               = local.send
    consume            = local.consume
    "send-and-consume" = sort(distinct(concat(local.send, local.consume)))
  }
}

data "aws_iam_policy_document" "this" {
  statement {
    sid       = "Queue"
    actions   = local.actions[var.access]
    resources = [var.queue_arn]
  }
}

resource "aws_iam_policy" "this" {
  name        = var.name
  path        = var.iam_path
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
