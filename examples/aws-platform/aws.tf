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

# What the modules in `examples/modules` create and change. The role creates
# the execution roles of functions and the policies attached to them, so it
# can grant those roles anything; give it to openporch alone.
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
    sid       = "Images"
    actions   = ["ecr:BatchGetImage", "ecr:DescribeImages", "ecr:GetDownloadUrlForLayer"]
    resources = ["${local.arn}:ecr:*:${local.account}:repository/*"]
  }
  statement {
    sid = "IdentityRoles"
    actions = [
      "iam:CreateRole", "iam:DeleteRole", "iam:GetRole", "iam:UpdateRole", "iam:UpdateAssumeRolePolicy",
      "iam:TagRole", "iam:UntagRole", "iam:ListRoleTags", "iam:ListInstanceProfilesForRole",
      "iam:PutRolePolicy", "iam:GetRolePolicy", "iam:DeleteRolePolicy", "iam:ListRolePolicies",
      "iam:AttachRolePolicy", "iam:DetachRolePolicy", "iam:ListAttachedRolePolicies",
    ]
    resources = ["${local.arn}:iam::${local.account}:role/*"]
  }
  statement {
    sid = "IdentityPolicies"
    actions = [
      "iam:CreatePolicy", "iam:DeletePolicy", "iam:GetPolicy", "iam:CreatePolicyVersion", "iam:DeletePolicyVersion",
      "iam:GetPolicyVersion", "iam:ListPolicyVersions", "iam:TagPolicy", "iam:UntagPolicy", "iam:ListPolicyTags",
    ]
    resources = ["${local.arn}:iam::${local.account}:policy/*"]
  }
  statement {
    sid       = "PassRolesToFunctionsAndSchedules"
    actions   = ["iam:PassRole"]
    resources = ["${local.arn}:iam::${local.account}:role/*"]
    condition {
      test     = "StringEquals"
      variable = "iam:PassedToService"
      values   = ["lambda.amazonaws.com", "scheduler.amazonaws.com"]
    }
  }
  statement {
    sid       = "NotItsOwnRole"
    effect    = "Deny"
    actions   = ["iam:*"]
    resources = [aws_iam_role.openporch.arn]
  }
}

resource "aws_iam_role_policy" "openporch" {
  name   = "openporch"
  role   = aws_iam_role.openporch.id
  policy = data.aws_iam_policy_document.openporch.json
}

# --- Function artifacts -----------------------------------------------------

# CI uploads zips here; deployments pin each to its object version, so a
# version stays usable for rollbacks and replays until the lifecycle rule
# below expires it after it stops being current.
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
    id     = "expire-replaced-versions"
    status = "Enabled"
    filter {}
    noncurrent_version_expiration {
      noncurrent_days = var.artifact_noncurrent_days
    }
    abort_incomplete_multipart_upload {
      days_after_initiation = 7
    }
  }

  depends_on = [aws_s3_bucket_versioning.artifacts]
}
