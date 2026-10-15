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
