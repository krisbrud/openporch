# What the platform needs in AWS: the role openporch assumes to provision
# workloads and their dependencies, and the versioned bucket CI uploads function
# artifacts to.

data "aws_caller_identity" "current" {}
data "aws_partition" "current" {}

locals {
  account   = data.aws_caller_identity.current.account_id
  partition = data.aws_partition.current.partition
  arn       = "arn:${local.partition}"
  buckets   = "${var.name_prefix}-${join("", [for i in range(32) : "?"])}"
  # Where the roles and policies openporch creates live.
  iam_path = "/openporch/"
  roles    = "${local.arn}:iam::${local.account}:role${local.iam_path}*"
  policies = "${local.arn}:iam::${local.account}:policy${local.iam_path}*"
}

# --- The role openporch assumes ---------------------------------------------

data "aws_iam_policy_document" "trust" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "AWS"
      identifiers = [var.trusted_principal_arn]
    }
    condition {
      test     = "StringEquals"
      variable = "sts:ExternalId"
      values   = [var.external_id]
    }
  }
}

resource "aws_iam_role" "openporch" {
  name                 = "${var.name_prefix}-platform"
  assume_role_policy   = data.aws_iam_policy_document.trust.json
  max_session_duration = 3600
}

# What the modules in `examples/modules` create and change. The account
# should hold openporch's resources alone: the role may change every function,
# table, queue and log group of a function in it.
#
# IAM is fenced in: the roles and policies openporch creates live under the
# path `/openporch/`, each role must carry the `boundary` policy below, which
# caps what any of them may do, and only policies under that path may be
# attached. So openporch can give a function access to data, but not to IAM
# or to roles and policies it did not create.
data "aws_iam_policy_document" "openporch" {
  statement {
    sid       = "Functions"
    actions   = ["lambda:*"]
    resources = ["${local.arn}:lambda:*:${local.account}:function:*", "${local.arn}:lambda:*:${local.account}:event-source-mapping:*"]
  }
  statement {
    sid       = "EventSourceMappings"
    actions   = ["lambda:CreateEventSourceMapping", "lambda:GetEventSourceMapping", "lambda:ListEventSourceMappings"]
    resources = ["*"]
  }
  statement {
    sid       = "Logs"
    actions   = ["logs:*"]
    resources = ["${local.arn}:logs:*:${local.account}:log-group:/aws/lambda/*"]
  }
  statement {
    sid       = "DescribeLogs"
    actions   = ["logs:DescribeLogGroups"]
    resources = ["*"]
  }
  statement {
    sid       = "Schedules"
    actions   = ["scheduler:*"]
    resources = ["${local.arn}:scheduler:*:${local.account}:schedule/default/*"]
  }
  statement {
    sid       = "SecretVariables"
    actions   = ["ssm:*"]
    resources = ["${local.arn}:ssm:*:${local.account}:parameter/openporch/*"]
  }
  statement {
    sid       = "DescribeParameters"
    actions   = ["ssm:DescribeParameters"]
    resources = ["*"]
  }
  statement {
    sid       = "SecretManagerVariables"
    actions   = ["secretsmanager:*"]
    resources = ["${local.arn}:secretsmanager:*:${local.account}:secret:openporch/*"]
  }
  statement {
    sid       = "Tables"
    actions   = ["dynamodb:*"]
    resources = ["${local.arn}:dynamodb:*:${local.account}:table/*"]
  }
  statement {
    sid       = "Queues"
    actions   = ["sqs:*"]
    resources = ["${local.arn}:sqs:*:${local.account}:*"]
  }
  statement {
    # Workloads' buckets are named `<prefix>-<guresid>`, 32 hex digits, which
    # the artifact bucket's name does not match.
    sid       = "Buckets"
    actions   = ["s3:*"]
    resources = ["${local.arn}:s3:::${local.buckets}", "${local.arn}:s3:::${local.buckets}/*"]
  }
  statement {
    sid       = "ReadArtifacts"
    actions   = ["s3:GetObject", "s3:GetObjectVersion", "s3:GetBucketVersioning", "s3:ListBucket"]
    resources = [aws_s3_bucket.artifacts.arn, "${aws_s3_bucket.artifacts.arn}/*"]
  }
  statement {
    # Lambda adds its own pull permission to a repository's policy when it
    # creates an image function.
    sid       = "Images"
    actions   = ["ecr:BatchGetImage", "ecr:DescribeImages", "ecr:GetDownloadUrlForLayer", "ecr:GetRepositoryPolicy", "ecr:SetRepositoryPolicy"]
    resources = ["${local.arn}:ecr:*:${local.account}:repository/*"]
  }
  statement {
    sid       = "CreateBoundedRoles"
    actions   = ["iam:CreateRole", "iam:PutRolePolicy"]
    resources = [local.roles]
    condition {
      test     = "StringEquals"
      variable = "iam:PermissionsBoundary"
      values   = [aws_iam_policy.boundary.arn]
    }
  }
  statement {
    sid       = "AttachOwnPolicies"
    actions   = ["iam:AttachRolePolicy"]
    resources = [local.roles]
    condition {
      test     = "StringEquals"
      variable = "iam:PermissionsBoundary"
      values   = [aws_iam_policy.boundary.arn]
    }
    condition {
      test     = "ArnLike"
      variable = "iam:PolicyARN"
      values   = [local.policies]
    }
  }
  statement {
    sid = "ManageRoles"
    actions = [
      "iam:DeleteRole", "iam:GetRole", "iam:TagRole", "iam:UntagRole", "iam:ListRoleTags",
      "iam:ListInstanceProfilesForRole", "iam:GetRolePolicy", "iam:DeleteRolePolicy", "iam:ListRolePolicies",
      "iam:DetachRolePolicy", "iam:ListAttachedRolePolicies",
    ]
    resources = [local.roles]
  }
  statement {
    sid = "ManagePolicies"
    actions = [
      "iam:CreatePolicy", "iam:DeletePolicy", "iam:GetPolicy", "iam:CreatePolicyVersion", "iam:DeletePolicyVersion",
      "iam:GetPolicyVersion", "iam:ListPolicyVersions", "iam:TagPolicy", "iam:UntagPolicy", "iam:ListPolicyTags",
    ]
    resources = [local.policies]
  }
  statement {
    sid       = "PassRolesToFunctionsAndSchedules"
    actions   = ["iam:PassRole"]
    resources = [local.roles]
    condition {
      test     = "StringEquals"
      variable = "iam:PassedToService"
      values   = ["lambda.amazonaws.com", "scheduler.amazonaws.com"]
    }
  }
}

# The most any role openporch creates may do: use data and invoke functions,
# never IAM. The policies openporch attaches grant the actual access.
data "aws_iam_policy_document" "boundary" {
  statement {
    sid = "Data"
    actions = [
      "dynamodb:*", "sqs:*", "s3:*", "logs:CreateLogStream", "logs:PutLogEvents", "lambda:InvokeFunction",
      "ssm:GetParameter", "ssm:GetParameters", "secretsmanager:GetSecretValue", "kms:Decrypt",
      "xray:PutTraceSegments", "xray:PutTelemetryRecords",
    ]
    resources = ["*"]
  }
}

resource "aws_iam_policy" "boundary" {
  name        = "${var.name_prefix}-boundary"
  description = "The most a role openporch creates may do."
  policy      = data.aws_iam_policy_document.boundary.json
}

resource "aws_iam_role_policy" "openporch" {
  name   = "openporch"
  role   = aws_iam_role.openporch.id
  policy = data.aws_iam_policy_document.openporch.json
}

# --- Function artifacts -----------------------------------------------------

# CI uploads zips here; deployments pin each to its object version, which
# stays usable for rollbacks and replays: replaced and deleted versions are
# kept unless `artifact_noncurrent_days` says otherwise.
resource "aws_s3_bucket" "artifacts" {
  bucket = "${var.name_prefix}-artifacts-${local.account}"
}

resource "aws_s3_bucket_versioning" "artifacts" {
  bucket = aws_s3_bucket.artifacts.id
  versioning_configuration {
    status = "Enabled"
  }
}

resource "aws_s3_bucket_public_access_block" "artifacts" {
  bucket                  = aws_s3_bucket.artifacts.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_lifecycle_configuration" "artifacts" {
  bucket = aws_s3_bucket.artifacts.id

  rule {
    id     = "abort-incomplete-uploads"
    status = "Enabled"
    filter {}
    abort_incomplete_multipart_upload {
      days_after_initiation = 7
    }
  }

  dynamic "rule" {
    for_each = var.artifact_noncurrent_days == null ? [] : [var.artifact_noncurrent_days]
    content {
      id     = "expire-replaced-versions"
      status = "Enabled"
      filter {}
      noncurrent_version_expiration {
        noncurrent_days = rule.value
      }
    }
  }

  depends_on = [aws_s3_bucket_versioning.artifacts]
}
