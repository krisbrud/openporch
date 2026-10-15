# The `aws-policy` a `dynamodb-table` definition co-provisions: a managed
# policy that reads and writes one table and its indexes, for a workload's
# execution role to attach.

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

variable "table_arn" {
  type        = string
  description = "The table's ARN, such as `$${resources.dynamodb-table.outputs.arn}`."

  validation {
    condition     = can(regex("^arn:aws[a-z-]*:dynamodb:[a-z0-9-]+:[0-9]{12}:table/[A-Za-z0-9_.-]+$", var.table_arn))
    error_message = "`table_arn` must be a DynamoDB table ARN."
  }
}

variable "read_only" {
  type        = bool
  default     = false
  description = "Grant only reads."
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
  read = [
    "dynamodb:BatchGetItem",
    "dynamodb:ConditionCheckItem",
    "dynamodb:DescribeTable",
    "dynamodb:GetItem",
    "dynamodb:Query",
    "dynamodb:Scan",
  ]
  write = [
    "dynamodb:BatchWriteItem",
    "dynamodb:DeleteItem",
    "dynamodb:PutItem",
    "dynamodb:UpdateItem",
  ]
}

data "aws_iam_policy_document" "this" {
  statement {
    sid       = "Table"
    actions   = var.read_only ? local.read : concat(local.read, local.write)
    resources = [var.table_arn, "${var.table_arn}/index/*"]
  }
  # Reading the table's stream, if it has one.
  statement {
    sid = "Stream"
    actions = [
      "dynamodb:DescribeStream",
      "dynamodb:GetRecords",
      "dynamodb:GetShardIterator",
      "dynamodb:ListStreams",
    ]
    resources = ["${var.table_arn}/stream/*"]
  }
}

resource "aws_iam_policy" "this" {
  name        = var.name
  path        = var.iam_path
  description = "Access to the DynamoDB table ${var.table_arn}"
  policy      = data.aws_iam_policy_document.this.json
  tags        = var.tags
}

output "arn" {
  value = aws_iam_policy.this.arn
}

output "document" {
  value = data.aws_iam_policy_document.this.json
}
