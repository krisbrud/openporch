# The `aws-policy` an `s3` definition co-provisions: a managed policy that lists
# one bucket and reads, writes and deletes its objects.

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

variable "bucket_arn" {
  type        = string
  description = "The bucket's ARN, such as `$${resources.s3.outputs.arn}`."

  validation {
    condition     = can(regex("^arn:aws[a-z-]*:s3:::[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$", var.bucket_arn))
    error_message = "`bucket_arn` must be an S3 bucket ARN."
  }
}

variable "read_only" {
  type        = bool
  default     = false
  description = "Grant only listing and reads."
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

data "aws_iam_policy_document" "this" {
  statement {
    sid       = "Bucket"
    actions   = ["s3:GetBucketLocation", "s3:ListBucket"]
    resources = [var.bucket_arn]
  }
  statement {
    sid       = "Objects"
    actions   = var.read_only ? ["s3:GetObject"] : ["s3:DeleteObject", "s3:GetObject", "s3:PutObject"]
    resources = ["${var.bucket_arn}/*"]
  }
}

resource "aws_iam_policy" "this" {
  name        = var.name
  path        = var.iam_path
  description = "Access to the S3 bucket ${var.bucket_arn}"
  policy      = data.aws_iam_policy_document.this.json
  tags        = var.tags
}

output "arn" {
  value = aws_iam_policy.this.arn
}

output "document" {
  value = data.aws_iam_policy_document.this.json
}
