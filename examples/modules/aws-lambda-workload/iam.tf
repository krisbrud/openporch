# The function's execution role: its logs, its secrets, the queues that
# trigger it, and the policies its dependencies' definitions co-provision.

data "aws_iam_policy_document" "assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "this" {
  name               = substr("${local.name}-lambda", 0, 64)
  assume_role_policy = data.aws_iam_policy_document.assume.json
  tags               = var.tags
}

data "aws_iam_policy_document" "this" {
  statement {
    sid       = "Logs"
    actions   = ["logs:CreateLogStream", "logs:PutLogEvents"]
    resources = ["${aws_cloudwatch_log_group.this.arn}:*"]
  }

  dynamic "statement" {
    for_each = length(aws_ssm_parameter.variable) > 0 ? [1] : []
    content {
      sid       = "SecretParameters"
      actions   = ["ssm:GetParameter", "ssm:GetParameters"]
      resources = [for p in aws_ssm_parameter.variable : p.arn]
    }
  }
  dynamic "statement" {
    # SecureString parameters use the account's `aws/ssm` key.
    for_each = length(aws_ssm_parameter.variable) > 0 ? [1] : []
    content {
      sid       = "DecryptSecretParameters"
      actions   = ["kms:Decrypt"]
      resources = ["arn:${data.aws_partition.current.partition}:kms:${local.region}:${data.aws_caller_identity.current.account_id}:key/*"]
      condition {
        test     = "StringEquals"
        variable = "kms:ViaService"
        values   = ["ssm.${local.region}.amazonaws.com"]
      }
    }
  }
  dynamic "statement" {
    for_each = length(aws_secretsmanager_secret.variable) > 0 ? [1] : []
    content {
      sid       = "Secrets"
      actions   = ["secretsmanager:GetSecretValue"]
      resources = [for s in aws_secretsmanager_secret.variable : s.arn]
    }
  }
  dynamic "statement" {
    for_each = length(local.queue_triggers) > 0 ? [1] : []
    content {
      sid = "TriggerQueues"
      actions = [
        "sqs:ChangeMessageVisibility",
        "sqs:DeleteMessage",
        "sqs:GetQueueAttributes",
        "sqs:ReceiveMessage",
      ]
      resources = [for t in values(local.queue_triggers) : t.arn]
    }
  }
}

resource "aws_iam_role_policy" "this" {
  name   = "openporch"
  role   = aws_iam_role.this.id
  policy = data.aws_iam_policy_document.this.json
}

resource "aws_iam_role_policy_attachment" "dependency" {
  for_each   = local.policies
  role       = aws_iam_role.this.name
  policy_arn = each.value
}
