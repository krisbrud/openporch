# A `dynamodb-table` resource: an on-demand DynamoDB table.
#
# The key and indexes come from the resource's inputs, the `params` a workload
# gives the resource, which the OpenTofu driver passes as `var.resource`:
#
#   resources:
#     orders:
#       type: dynamodb-table
#       params:
#         primaryKey: {name: id, type: S}
#         sortKey: {name: created, type: N}
#         indexes:
#           by-customer: {primaryKey: {name: customer, type: S}}
#
# A key is `{name, type}`, where type is `S` (string, the default), `N`
# (number) or `B` (binary); `attributes` may give types by name instead, as
# `{customer: S}`. Without params the table has the string key `id`.
# `stream` (`NEW_IMAGE`, `OLD_IMAGE`, `NEW_AND_OLD_IMAGES` or `KEYS_ONLY`)
# turns on a stream and sets the `stream_arn` output.

terraform {
  required_version = ">= 1.8"
  required_providers {
    aws = { source = "hashicorp/aws", version = "~> 6.0" }
  }
}

variable "name" {
  type        = string
  description = "The table name, unique in the account and region, such as `$${context.app.id}-$${context.env.id}-$${context.res.id}` with the dots of the resource ID replaced."

  validation {
    condition     = can(regex("^[A-Za-z0-9_.-]{3,255}$", var.name))
    error_message = "A table name has 3 to 255 letters, digits, `_`, `-` and `.`."
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
  params     = var.resource
  attributes = try(local.params.attributes, {})

  # A key from `{name, type}`, with its type from `attributes` or `S`.
  primary = try(local.params.primaryKey, { name = "id" })
  sort    = try(local.params.sortKey, null)
  indexes = {
    for name, index in try(local.params.indexes, {}) : name => {
      hash  = index.primaryKey.name
      range = try(index.sortKey.name, null)
      # `ALL`, `KEYS_ONLY` or `INCLUDE` with `attributes`.
      projection = try(index.projection, "ALL")
      include    = try(index.include, null)
    }
  }
  keys = concat(
    [local.primary],
    local.sort == null ? [] : [local.sort],
    flatten([
      for index in values(try(local.params.indexes, {})) :
      concat([index.primaryKey], try([index.sortKey], []))
    ]),
  )
  key_types = {
    for key in local.keys : key.name => try(key.type, local.attributes[key.name], "S")...
  }
  key_type = { for name, types in local.key_types : name => types[0] }
  stream   = try(local.params.stream, null)
}

resource "aws_dynamodb_table" "this" {
  name         = var.name
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = local.primary.name
  range_key    = try(local.sort.name, null)

  dynamic "attribute" {
    for_each = local.key_type
    content {
      name = attribute.key
      type = attribute.value
    }
  }

  dynamic "global_secondary_index" {
    for_each = local.indexes
    content {
      name               = global_secondary_index.key
      projection_type    = global_secondary_index.value.projection
      non_key_attributes = global_secondary_index.value.include

      key_schema {
        attribute_name = global_secondary_index.value.hash
        key_type       = "HASH"
      }
      dynamic "key_schema" {
        for_each = global_secondary_index.value.range == null ? [] : [global_secondary_index.value.range]
        content {
          attribute_name = key_schema.value
          key_type       = "RANGE"
        }
      }
    }
  }

  stream_enabled   = local.stream != null
  stream_view_type = local.stream

  point_in_time_recovery {
    enabled = try(local.params.pointInTimeRecovery, false)
  }

  server_side_encryption {
    enabled = true
  }

  tags = var.tags

  lifecycle {
    precondition {
      condition     = alltrue([for types in values(local.key_types) : length(distinct(types)) == 1])
      error_message = "Each key attribute must have one type wherever it is used."
    }
    precondition {
      condition     = alltrue([for t in values(local.key_type) : contains(["S", "N", "B"], t)])
      error_message = "Key types are `S`, `N` or `B`."
    }
  }
}

data "aws_region" "current" {}

output "name" {
  value = aws_dynamodb_table.this.name
}

output "arn" {
  value = aws_dynamodb_table.this.arn
}

output "region" {
  value = data.aws_region.current.region
}

output "stream_arn" {
  value = aws_dynamodb_table.this.stream_arn
}
