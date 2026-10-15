# An `s3` resource: a private, encrypted bucket.
#
# Settings come from the resource's inputs, the `params` a workload gives the
# resource, which the OpenTofu driver passes as `var.resource`:
#
#   resources:
#     uploads:
#       type: s3
#       params:
#         versioning: true
#         expireAfterDays: 30   # deletes objects (and old versions) after 30 days
#
# Deleting the resource deletes the bucket with every object in it, after the
# deletion is acknowledged like any other destructive change.

terraform {
  required_version = ">= 1.8"
  required_providers {
    aws = { source = "hashicorp/aws", version = "~> 6.0" }
  }
}

variable "name" {
  type        = string
  description = "The bucket name, unique across AWS, such as `acme-$${context.res.guresid}`."

  validation {
    condition     = can(regex("^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$", var.name)) && !can(regex("\\.\\.", var.name))
    error_message = "A bucket name has 3 to 63 lowercase letters, digits, `.` and `-`, and starts and ends with a letter or digit."
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
  versioning = try(tobool(local.params.versioning), false)
  expire     = try(tonumber(local.params.expireAfterDays), null)
}

resource "aws_s3_bucket" "this" {
  bucket        = var.name
  force_destroy = true
  tags          = var.tags
}

resource "aws_s3_bucket_public_access_block" "this" {
  bucket                  = aws_s3_bucket.this.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_ownership_controls" "this" {
  bucket = aws_s3_bucket.this.id
  rule {
    object_ownership = "BucketOwnerEnforced"
  }
}

resource "aws_s3_bucket_server_side_encryption_configuration" "this" {
  bucket = aws_s3_bucket.this.id
  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}

resource "aws_s3_bucket_versioning" "this" {
  count  = local.versioning ? 1 : 0
  bucket = aws_s3_bucket.this.id
  versioning_configuration {
    status = "Enabled"
  }
}

resource "aws_s3_bucket_lifecycle_configuration" "this" {
  count  = local.expire == null ? 0 : 1
  bucket = aws_s3_bucket.this.id
  rule {
    id     = "expire"
    status = "Enabled"
    filter {}
    expiration {
      days = local.expire
    }
    noncurrent_version_expiration {
      noncurrent_days = local.expire
    }
  }

  lifecycle {
    precondition {
      condition     = try(local.expire >= 1, false)
      error_message = "`expireAfterDays` is a whole number of days, at least 1."
    }
  }
}

data "aws_region" "current" {}

output "bucket" {
  value = aws_s3_bucket.this.bucket
}

output "region" {
  value = data.aws_region.current.region
}

output "arn" {
  value = aws_s3_bucket.this.arn
}
