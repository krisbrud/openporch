# An `sns-topic` resource: a standard or FIFO topic that functions publish to,
# and queues or functions subscribe to.
#
# Settings come from the resource's inputs, the `params` a workload gives the
# resource, which the OpenTofu driver passes as `var.resource`:
#
#   resources:
#     events:
#       type: sns-topic
#       params:
#         fifo: true                       # ordered, deduplicated delivery
#         contentBasedDeduplication: true  # FIFO only: deduplicate by body
#
# Topics are not encrypted with a KMS key: publishers would also need to use the
# key, which the co-provisioned policy and the platform's boundary do not grant.

terraform {
  required_version = ">= 1.8"
  required_providers {
    aws = { source = "hashicorp/aws", version = "~> 6.0" }
  }
}

variable "name" {
  type        = string
  description = "The topic name, unique in the account and region; `.fifo` is added for a FIFO topic."

  validation {
    condition     = can(regex("^[A-Za-z0-9_-]{1,251}$", var.name))
    error_message = "A topic name has 1 to 251 letters, digits, `_` and `-`, which leaves room for `.fifo`."
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
  params = var.resource
  fifo   = try(tobool(local.params.fifo), false)
  dedup  = try(tobool(local.params.contentBasedDeduplication), false)
}

resource "aws_sns_topic" "this" {
  name                        = local.fifo ? "${var.name}.fifo" : var.name
  fifo_topic                  = local.fifo
  content_based_deduplication = local.fifo ? local.dedup : null
  tags                        = var.tags

  lifecycle {
    precondition {
      condition     = local.fifo || !local.dedup
      error_message = "`contentBasedDeduplication` needs `fifo: true`."
    }
  }
}

data "aws_region" "current" {}

output "arn" {
  value = aws_sns_topic.this.arn
}

output "name" {
  value = aws_sns_topic.this.name
}

output "region" {
  value = data.aws_region.current.region
}
