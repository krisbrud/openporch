# An `sqs` resource: a standard or FIFO queue, with an optional dead-letter
# queue.
#
# Settings come from the resource's inputs, the `params` a workload gives the
# resource, which the OpenTofu driver passes as `var.resource`:
#
#   resources:
#     inbox:
#       type: sqs
#       params:
#         visibilityTimeout: 120   # seconds; at least the consuming function's timeout
#         maxReceiveCount: 5       # adds a dead-letter queue
#
# `fifo: true` makes a FIFO queue; `messageRetention` is in seconds.

terraform {
  required_version = ">= 1.8"
  required_providers {
    aws = { source = "hashicorp/aws", version = "~> 6.0" }
  }
}

variable "name" {
  type        = string
  description = "The queue name, unique in the account and region; `.fifo` is added for a FIFO queue."

  validation {
    condition     = can(regex("^[A-Za-z0-9_-]{1,75}$", var.name))
    error_message = "A queue name has 1 to 75 letters, digits, `_` and `-`."
  }
}

variable "resource" {
  type        = any
  default     = {}
  nullable    = false
  description = "The resource's inputs, set by the OpenTofu driver."
}

variable "tags" {
  type    = map(string)
  default = {}
}

locals {
  params    = var.resource
  fifo      = try(tobool(local.params.fifo), false)
  suffix    = local.fifo ? ".fifo" : ""
  retention = try(tonumber(local.params.messageRetention), 345600)
  max       = try(tonumber(local.params.maxReceiveCount), null)
}

resource "aws_sqs_queue" "dead_letter" {
  count                     = local.max == null ? 0 : 1
  name                      = "${var.name}-dlq${local.suffix}"
  fifo_queue                = local.fifo
  message_retention_seconds = 1209600
  sqs_managed_sse_enabled   = true
  tags                      = var.tags
}

resource "aws_sqs_queue" "this" {
  name                       = "${var.name}${local.suffix}"
  fifo_queue                 = local.fifo
  visibility_timeout_seconds = try(tonumber(local.params.visibilityTimeout), 30)
  message_retention_seconds  = local.retention
  sqs_managed_sse_enabled    = true
  redrive_policy = local.max == null ? null : jsonencode({
    deadLetterTargetArn = aws_sqs_queue.dead_letter[0].arn
    maxReceiveCount     = local.max
  })
  tags = var.tags

  lifecycle {
    precondition {
      condition     = local.retention >= 60 && local.retention <= 1209600
      error_message = "`messageRetention` is 60 to 1209600 seconds."
    }
    precondition {
      condition     = local.max == null || try(local.max >= 1 && local.max <= 1000, false)
      error_message = "`maxReceiveCount` is 1 to 1000."
    }
  }
}

data "aws_region" "current" {}

output "arn" {
  value = aws_sqs_queue.this.arn
}

output "url" {
  value = aws_sqs_queue.this.url
}

output "region" {
  value = data.aws_region.current.region
}
