# A `workload` resource as one AWS Lambda function, for the `aws-lambda` class.
#
# The OpenTofu driver passes the resolved workload document as `var.resource`
# (`id`, `spec` and the outputs of its `dependencies`), and the values that
# resolved from secrets as `var.resource_secrets`, at the same paths. The
# function is named `<app>-<env>-<workload>`.
#
# - A function workload (`spec.function`) is a zip function from an S3
#   artifact, `s3://bucket/key?versionId=...`, or an image function.
# - A container workload with exactly one container becomes an image function:
#   its `command` and `args` override the image's entry point and command, and
#   the HTTP trigger's service port is exported as `AWS_LWA_PORT` for the Lambda
#   Web Adapter, which the image must include.
# - Images must be in ECR, in the function's region.
# - Plain variables become environment variables. A secret variable `NAME` is
#   stored as an SSM SecureString parameter (or a Secrets Manager secret with
#   `target.lambda.secrets: secretsmanager`) that the function may read, and
#   the environment has `NAME_SECRET_REF` with its name instead. Secrets
#   anywhere else in the workload's spec are refused, never dropped.
# - The execution role attaches the `arn` of every `aws-policy` among the
#   workload's dependencies, which the definitions of its tables, queues and
#   buckets co-provision.
# - Triggers: `http` is a function URL with explicit `auth: none` or `iam`;
#   `schedule` is an EventBridge Scheduler schedule from a 5-field cron
#   expression; `queue` is an SQS event source mapping. `topic` and `stream`
#   are not supported yet.
#
# `target.lambda` takes `secrets` (`ssm` or `secretsmanager`) and
# `architecture` (`x86_64` or `arm64`).

terraform {
  required_version = ">= 1.8"
  required_providers {
    # 6.28 added `invoked_via_function_url` to `aws_lambda_permission`.
    aws = { source = "hashicorp/aws", version = ">= 6.28, < 7.0" }
  }
}

data "aws_region" "current" {}
data "aws_caller_identity" "current" {}

locals {
  region = data.aws_region.current.region

  id           = try(tostring(var.resource.id), "")
  name         = "${var.app}-${var.env}-${local.id}"
  spec         = try(var.resource.spec, {})
  dependencies = try(var.resource.dependencies, {})
  target       = try(local.spec.target.lambda, {})

  # A function workload, or a container workload with one container.
  function       = try(local.spec.function, null)
  is_function    = local.function != null
  containers     = try(local.spec.containers, {})
  container_id   = local.is_function ? null : try(keys(local.containers)[0], null)
  container      = local.container_id == null ? null : local.containers[local.container_id]
  package        = try(local.function.package, {})
  package_type   = try(local.package.type, null)
  is_zip         = local.is_function && local.package_type == "zip"
  image          = local.is_zip ? null : (local.is_function ? try(local.package.image, null) : try(local.container.image, null))
  image_in_ecr   = local.image == null || can(regex("^[0-9]{12}\\.dkr\\.ecr\\.${local.region}\\.amazonaws\\.com/", local.image))
  artifact       = local.is_zip ? try(regex("^s3://([^/?]+)/([^?]+)(?:\\?versionId=(.+))?$", local.package.artifact), null) : null
  artifact_valid = !local.is_zip || local.artifact != null

  # Memory as `256Mi` or `1Gi` (Lambda's MB are MiB); timeout as `30s`, `5m`
  # or `1m30s`.
  memory_parts = try(regex("^([0-9]+)(Mi|M|Gi|G)?$", try(local.function.memory, "256Mi")), null)
  memory_mb    = local.memory_parts == null ? 0 : tonumber(local.memory_parts[0]) * (contains(["Gi", "G"], coalesce(local.memory_parts[1], "Mi")) ? 1024 : 1)
  timeout_text = try(local.function.timeout, "30s")
  timeout_parts = can(regex("^([0-9]+(h|m|s))+$", local.timeout_text)) ? [
    for p in regexall("([0-9]+)(h|m|s)", local.timeout_text) : tonumber(p[0]) * lookup({ h = 3600, m = 60, s = 1 }, p[1])
  ] : []
  timeout_seconds = length(local.timeout_parts) == 0 ? 0 : sum(local.timeout_parts)
  # Concurrency that is not a whole number fails its precondition as -2 or 0.
  reserved_text    = try(local.function.concurrency.reserved, null)
  reserved         = local.reserved_text == null ? -1 : try(tonumber(local.reserved_text) == floor(tonumber(local.reserved_text)) ? tonumber(local.reserved_text) : -2, -2)
  provisioned_text = try(local.function.concurrency.provisioned, null)
  provisioned      = local.provisioned_text == null ? null : try(tonumber(local.provisioned_text) == floor(tonumber(local.provisioned_text)) ? tonumber(local.provisioned_text) : 0, 0)

  # Variables: plain ones as they are, secret ones by reference.
  plain_variables = local.is_function ? try(local.function.variables, {}) : try(local.container.variables, {})
  secret_values = local.is_function ? try(var.resource_secrets.spec.function.variables, {}) : try(
    var.resource_secrets.spec.containers[local.container_id].variables, {}
  )
  secret_names   = nonsensitive(toset(keys(local.secret_values)))
  secret_backend = try(local.target.secrets, "ssm")
  secret_refs = {
    for n in local.secret_names : "${n}_SECRET_REF" => (
      local.secret_backend == "secretsmanager" ? aws_secretsmanager_secret.variable[n].name : aws_ssm_parameter.variable[n].name
    )
  }
  # Secrets the function would not get: anywhere but the selected variables
  # and the dependencies' outputs, such as in `args` or a schedule's input.
  secret_spec = try(var.resource_secrets.spec, {})
  stray_secrets = nonsensitive(concat(
    [for k in keys(var.resource_secrets) : k if !contains(["spec", "dependencies"], k)],
    [for k in try(keys(local.secret_spec), ["?"]) : "spec.${k}" if k != (local.is_function ? "function" : "containers")],
    local.is_function ? [
      for k in try(keys(try(local.secret_spec.function, {})), ["?"]) : "spec.function.${k}" if k != "variables"
      ] : flatten([
        for c, v in try(local.secret_spec.containers, {}) : [
          for k in try(keys(v), ["?"]) : "spec.containers.${c}.${k}" if c != local.container_id || k != "variables"
        ]
    ]),
  ))

  # The service port a container listens on: the HTTP trigger's, or the first.
  ports     = try(local.spec.service.ports, {})
  http_port = try([for t in values(local.triggers) : t.http.port if can(t.http.port)][0], try(keys(local.ports)[0], null))
  lwa_port  = local.is_function || local.http_port == null ? {} : { AWS_LWA_PORT = tostring(try(local.ports[local.http_port].targetPort, local.ports[local.http_port].port)) }

  environment = merge(local.plain_variables, local.secret_refs, local.lwa_port)
  # Lambda measures its 4 KB limit over names and values.
  environment_size = sum(concat([0], [for k, v in local.environment : length(k) + length(tostring(v))]))
  # Names Lambda sets itself, and the Web Adapter's port, which the module sets.
  reserved_names = [
    "_HANDLER", "_X_AMZN_TRACE_ID", "AWS_ACCESS_KEY", "AWS_ACCESS_KEY_ID", "AWS_DEFAULT_REGION",
    "AWS_EXECUTION_ENV", "AWS_LAMBDA_FUNCTION_MEMORY_SIZE", "AWS_LAMBDA_FUNCTION_NAME",
    "AWS_LAMBDA_FUNCTION_VERSION", "AWS_LAMBDA_INITIALIZATION_TYPE", "AWS_LAMBDA_LOG_GROUP_NAME",
    "AWS_LAMBDA_LOG_STREAM_NAME", "AWS_LAMBDA_RUNTIME_API", "AWS_REGION", "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN", "LAMBDA_RUNTIME_DIR", "LAMBDA_TASK_ROOT",
  ]
  reserved_used = setintersection(
    concat(keys(local.plain_variables), tolist(local.secret_names)),
    concat(local.reserved_names, keys(local.lwa_port)),
  )

  # The policies its dependencies' definitions co-provision.
  policies = { for key, outputs in local.dependencies : key => outputs.arn if startswith(key, "aws-policy.") && can(outputs.arn) }
}

resource "aws_cloudwatch_log_group" "this" {
  name              = "/aws/lambda/${local.name}"
  retention_in_days = var.log_retention_days
  tags              = var.tags
}

resource "aws_ssm_parameter" "variable" {
  for_each = local.secret_backend == "ssm" ? local.secret_names : toset([])
  name     = "/openporch/${local.name}/${each.key}"
  type     = "SecureString"
  value    = local.secret_values[each.key]
  tags     = var.tags
}

resource "aws_secretsmanager_secret" "variable" {
  for_each                = local.secret_backend == "secretsmanager" ? local.secret_names : toset([])
  name                    = "openporch/${local.name}/${each.key}"
  recovery_window_in_days = 0
  tags                    = var.tags
}

resource "aws_secretsmanager_secret_version" "variable" {
  for_each      = aws_secretsmanager_secret.variable
  secret_id     = each.value.id
  secret_string = local.secret_values[each.key]
}

resource "aws_lambda_function" "this" {
  function_name = local.name
  role          = aws_iam_role.this.arn
  package_type  = local.is_zip ? "Zip" : "Image"
  architectures = [try(local.target.architecture, "x86_64")]

  s3_bucket         = local.is_zip ? local.artifact[0] : null
  s3_key            = local.is_zip ? local.artifact[1] : null
  s3_object_version = local.is_zip ? local.artifact[2] : null
  runtime           = local.is_zip ? try(local.package.runtime, null) : null
  handler           = local.is_zip ? try(local.package.handler, null) : null
  image_uri         = local.image

  dynamic "image_config" {
    for_each = local.container != null && (try(length(local.container.command), 0) > 0 || try(length(local.container.args), 0) > 0) ? [local.container] : []
    content {
      entry_point = try(image_config.value.command, null)
      command     = try(image_config.value.args, null)
    }
  }

  memory_size                    = local.memory_mb
  timeout                        = local.timeout_seconds
  reserved_concurrent_executions = local.reserved
  publish                        = local.provisioned != null

  dynamic "environment" {
    for_each = length(local.environment) > 0 ? [local.environment] : []
    content {
      variables = environment.value
    }
  }

  logging_config {
    log_format = "Text"
    log_group  = aws_cloudwatch_log_group.this.name
  }

  tags = var.tags

  depends_on = [
    aws_iam_role_policy.this,
    aws_iam_role_policy_attachment.dependency,
  ]

  lifecycle {
    precondition {
      condition     = local.id != "" && can(regex("^[A-Za-z0-9_-]{1,64}$", local.name))
      error_message = "The function name `${local.name}` must be 1 to 64 letters, digits, `-` and `_`; shorten the application, environment or workload ID."
    }
    precondition {
      condition     = local.is_function != (length(local.containers) > 0)
      error_message = "A Lambda workload is either a function or containers."
    }
    precondition {
      condition     = local.is_function || length(local.containers) == 1
      error_message = "A container workload on Lambda has exactly one container; this one has ${length(local.containers)}."
    }
    precondition {
      condition     = local.container == null || length(try(local.container.files, {})) == 0
      error_message = "Container `${coalesce(local.container_id, "-")}` has `files`, which Lambda cannot mount; build them into the image."
    }
    precondition {
      condition     = !local.is_function || contains(["zip", "image"], coalesce(local.package_type, "-"))
      error_message = "`function.package.type` is `zip` or `image`."
    }
    precondition {
      condition     = local.is_zip || local.image != null
      error_message = "An image function or container needs an `image`."
    }
    precondition {
      condition     = !local.is_zip || (try(local.package.runtime, null) != null && try(local.package.handler, null) != null)
      error_message = "A zip function needs `package.runtime` and `package.handler`."
    }
    precondition {
      condition     = local.image_in_ecr
      error_message = "Lambda container images must come from ECR in the function's region (${local.region}); `${coalesce(local.image, "-")}` does not."
    }
    precondition {
      condition     = local.artifact_valid
      error_message = "A zip function's artifact must be an S3 object, `s3://bucket/key`, optionally with `?versionId=...`."
    }
    precondition {
      condition     = local.is_function || local.http_port == null || length(local.lwa_port) == 1
      error_message = "The HTTP trigger's port `${coalesce(local.http_port, "-")}` is not a service port."
    }
    precondition {
      condition     = local.memory_mb >= 128 && local.memory_mb <= 10240
      error_message = "`function.memory` must be 128Mi to 10240Mi."
    }
    precondition {
      condition     = local.timeout_seconds >= 1 && local.timeout_seconds <= 900
      error_message = "`function.timeout` must be 1s to 15m, written like `30s` or `5m`."
    }
    precondition {
      condition     = local.reserved >= -1
      error_message = "`function.concurrency.reserved` must be a whole number, 0 or more."
    }
    precondition {
      condition     = local.provisioned == null || try(local.provisioned >= 1, false)
      error_message = "`function.concurrency.provisioned` must be a whole number, at least 1."
    }
    precondition {
      condition     = local.provisioned == null || local.reserved == -1 || try(local.provisioned <= local.reserved, false)
      error_message = "`function.concurrency.provisioned` cannot exceed `reserved`."
    }
    precondition {
      condition     = length(local.stray_secrets) == 0
      error_message = "Only variables and dependencies may hold secrets on Lambda; these would not reach the function: ${join(", ", local.stray_secrets)}."
    }
    precondition {
      condition     = length(local.reserved_used) == 0
      error_message = "These environment variable names are set by Lambda or by this module: ${join(", ", local.reserved_used)}."
    }
    precondition {
      condition     = length(setintersection(keys(local.plain_variables), keys(local.secret_refs))) == 0
      error_message = "A plain variable has the name of a secret's reference: ${join(", ", setintersection(keys(local.plain_variables), keys(local.secret_refs)))}."
    }
    precondition {
      condition     = local.environment_size <= 4096
      error_message = "The function's environment variables exceed Lambda's 4 KB limit."
    }
    precondition {
      condition     = contains(["ssm", "secretsmanager"], local.secret_backend)
      error_message = "`target.lambda.secrets` is `ssm` or `secretsmanager`."
    }
    precondition {
      condition     = contains(["x86_64", "arm64"], try(local.target.architecture, "x86_64"))
      error_message = "`target.lambda.architecture` is `x86_64` or `arm64`."
    }
    precondition {
      condition     = length(setsubtract(keys(try(local.spec.target, {})), ["lambda"])) == 0 && length(setsubtract(keys(local.target), ["secrets", "architecture"])) == 0
      error_message = "`target` takes only `lambda.secrets` and `lambda.architecture` on Lambda."
    }
  }
}

# With provisioned concurrency, triggers invoke the `live` alias, which points
# at the version each deployment publishes.
resource "aws_lambda_alias" "live" {
  count            = local.provisioned == null ? 0 : 1
  name             = "live"
  function_name    = aws_lambda_function.this.function_name
  function_version = aws_lambda_function.this.version
}

resource "aws_lambda_provisioned_concurrency_config" "live" {
  count                             = local.provisioned == null ? 0 : 1
  function_name                     = aws_lambda_function.this.function_name
  qualifier                         = aws_lambda_alias.live[0].name
  provisioned_concurrent_executions = local.provisioned
}

locals {
  invoke_arn = local.provisioned == null ? aws_lambda_function.this.arn : aws_lambda_alias.live[0].arn
  qualifier  = local.provisioned == null ? null : "live"
}
