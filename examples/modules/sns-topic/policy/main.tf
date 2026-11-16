# The `aws-policy` an `sns-topic` definition co-provisions: a managed policy
# that publishes to one topic, subscribes to it, or both.

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

variable "topic_arn" {
  type        = string
  description = "The topic's ARN, such as `$${resources.sns-topic.outputs.arn}`."

  validation {
    condition     = can(regex("^arn:aws[a-z-]*:sns:[a-z0-9-]+:[0-9]{12}:[A-Za-z0-9_-]+(\\.fifo)?$", var.topic_arn))
    error_message = "`topic_arn` must be an SNS topic ARN."
  }
}

variable "access" {
  type        = string
  default     = "publish"
  description = "What the policy grants: `publish`, `subscribe` or `publish-and-subscribe`."

  validation {
    condition     = contains(["publish", "subscribe", "publish-and-subscribe"], var.access)
    error_message = "`access` is `publish`, `subscribe` or `publish-and-subscribe`."
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
  publish   = ["sns:GetTopicAttributes", "sns:Publish"]
  subscribe = ["sns:GetTopicAttributes", "sns:ListSubscriptionsByTopic", "sns:Subscribe", "sns:Unsubscribe"]
  actions = {
    publish                 = local.publish
    subscribe               = local.subscribe
    "publish-and-subscribe" = sort(distinct(concat(local.publish, local.subscribe)))
  }
}

data "aws_iam_policy_document" "this" {
  statement {
    sid       = "Topic"
    actions   = local.actions[var.access]
    resources = [var.topic_arn]
  }
}

resource "aws_iam_policy" "this" {
  name        = var.name
  path        = var.iam_path
  description = "Access to the SNS topic ${var.topic_arn}"
  policy      = data.aws_iam_policy_document.this.json
  tags        = var.tags
}

output "arn" {
  value = aws_iam_policy.this.arn
}

output "document" {
  value = data.aws_iam_policy_document.this.json
}
