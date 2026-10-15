# The openporch side: environment types, the cloud account, resource classes,
# and the definitions that say how each resource type is provisioned. Nothing
# here needs Kubernetes.
#
# openporch placeholders are written $${...} so OpenTofu does not interpolate
# them.

# Every organization has `development`.
resource "openporch_environment_type" "production" {
  id          = "production"
  description = "Production environments"
}

resource "openporch_cloud_account" "aws" {
  id   = "aws"
  name = "AWS"
  type = "aws-role"
  config = {
    role_arn    = aws_iam_role.openporch.arn
    external_id = var.external_id
    region      = var.aws_region
  }
  depends_on = [aws_iam_role_policy.openporch]
}

locals {
  module = { url = var.modules_source, rev = var.modules_rev }
  # Tags on everything a module creates, saying whose it is.
  tags = {
    "openporch:app"      = "$${context.app.id}"
    "openporch:env"      = "$${context.env.id}"
    "openporch:resource" = "$${context.res.id}"
  }
  # A table's name from the application, environment and resource ID, such
  # as `shop.dev.modules.api.externals.orders`. Application and environment IDs
  # have no dots, so the name is unique.
  table_name = "$${context.app.id}.$${context.env.id}.$${context.res.id}"
}

# --- Environments and workloads ---------------------------------------------

resource "openporch_resource_definition" "base_env" {
  id          = "base-env"
  type        = "base-env"
  driver_type = "openporch/none"
}

resource "openporch_resource_definition_criteria" "base_env" {
  resource_definition_id = openporch_resource_definition.base_env.id
}

resource "openporch_resource_class" "aws_lambda" {
  resource_type = "workload"
  id            = "aws-lambda"
  description   = "A function on AWS Lambda"
}

# `class: aws-lambda` workloads run as Lambda functions.
resource "openporch_resource_definition" "lambda" {
  id             = "aws-lambda-workload"
  name           = "Lambda function"
  type           = "workload"
  driver_type    = "openporch/opentofu"
  driver_account = openporch_cloud_account.aws.id
  driver_inputs = {
    source = merge(local.module, { path = "examples/modules/aws-lambda-workload" })
    variables = {
      app                  = "$${context.app.id}"
      env                  = "$${context.env.id}"
      iam_path             = local.iam_path
      permissions_boundary = aws_iam_policy.boundary.arn
      tags                 = local.tags
    }
  }
}

resource "openporch_resource_definition_criteria" "lambda" {
  resource_definition_id = openporch_resource_definition.lambda.id
  class                  = openporch_resource_class.aws_lambda.id
}

# Workloads of the default class run nowhere: their resolved document is
# stored as their outputs, which suits trying a workload out.
resource "openporch_resource_definition" "workload_none" {
  id          = "workload-none"
  name        = "No runtime"
  type        = "workload"
  driver_type = "openporch/none"
}

resource "openporch_resource_definition_criteria" "workload_none" {
  resource_definition_id = openporch_resource_definition.workload_none.id
}

# --- Serverless dependencies ------------------------------------------------

# Each dependency co-provisions an `aws-policy` of a class named after its
# type, after the resource exists; the workloads that use it wait for the
# policy, whose ARN their execution role attaches.
resource "openporch_resource_class" "policy" {
  for_each      = toset(["dynamodb-table", "sqs", "s3"])
  resource_type = "aws-policy"
  id            = each.key
  description   = "Access to a ${each.key} resource"
}

locals {
  dependencies = {
    dynamodb-table = {
      name          = "DynamoDB table"
      module        = "examples/modules/dynamodb-table"
      resource_name = local.table_name
      # The class is named, since a reference without one means the policy's
      # own class. These definitions provision the `default` class.
      policy_arn = { table_arn = "$${resources.dynamodb-table.default.outputs.arn}" }
    }
    sqs = {
      name   = "SQS queue"
      module = "examples/modules/sqs"
      # Queue names take no dots, and the readable name may be too long.
      resource_name = "${var.name_prefix}-$${context.res.guresid}"
      policy_arn    = { queue_arn = "$${resources.sqs.default.outputs.arn}" }
    }
    s3 = {
      name   = "S3 bucket"
      module = "examples/modules/s3"
      # Bucket names are unique across AWS.
      resource_name = "${var.name_prefix}-$${context.res.guresid}"
      policy_arn    = { bucket_arn = "$${resources.s3.default.outputs.arn}" }
    }
  }
}

resource "openporch_resource_definition" "dependency" {
  for_each       = local.dependencies
  id             = each.key
  name           = each.value.name
  type           = each.key
  driver_type    = "openporch/opentofu"
  driver_account = openporch_cloud_account.aws.id
  driver_inputs = {
    source = merge(local.module, { path = each.value.module })
    variables = {
      name = each.value.resource_name
      tags = local.tags
    }
  }
  provision = {
    "aws-policy.${each.key}" = { match_dependents = true }
  }
  depends_on = [openporch_resource_class.policy]
}

resource "openporch_resource_definition_criteria" "dependency" {
  for_each               = local.dependencies
  resource_definition_id = openporch_resource_definition.dependency[each.key].id
}

resource "openporch_resource_definition" "policy" {
  for_each       = local.dependencies
  id             = "${each.key}-policy"
  name           = "${each.value.name} access"
  type           = "aws-policy"
  driver_type    = "openporch/opentofu"
  driver_account = openporch_cloud_account.aws.id
  driver_inputs = {
    source = merge(local.module, { path = "${each.value.module}/policy" })
    variables = merge(each.value.policy_arn, {
      name     = "${var.name_prefix}-$${context.res.guresid}"
      iam_path = local.iam_path
      tags     = local.tags
    })
  }
}

resource "openporch_resource_definition_criteria" "policy" {
  for_each               = local.dependencies
  resource_definition_id = openporch_resource_definition.policy[each.key].id
  class                  = openporch_resource_class.policy[each.key].id
}

# --- DNS --------------------------------------------------------------------

# A name per application and environment, reported by the echo driver; a real
# platform would create the record.
resource "openporch_resource_definition" "dns" {
  id          = "dns"
  type        = "dns"
  driver_type = "openporch/echo"
  driver_inputs = {
    host = "$${context.app.id}-$${context.env.id}.${var.dns_zone}"
  }
}

resource "openporch_resource_definition_criteria" "dns" {
  resource_definition_id = openporch_resource_definition.dns.id
}
