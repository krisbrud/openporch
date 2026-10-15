# Emulators such as Floci serve S3 at one host name, so buckets are addressed by
# path; real S3 accepts that too.
provider "aws" {
  s3_use_path_style = true
}

run "private_bucket" {
  variables {
    name = "openporch-test-uploads"
  }

  assert {
    condition     = aws_s3_bucket_public_access_block.this.block_public_policy && aws_s3_bucket_public_access_block.this.restrict_public_buckets
    error_message = "The bucket blocks public access."
  }
  assert {
    condition     = length(aws_s3_bucket_versioning.this) == 0 && length(aws_s3_bucket_lifecycle_configuration.this) == 0
    error_message = "Without params the bucket is unversioned and keeps objects."
  }
  assert {
    condition     = output.bucket == "openporch-test-uploads" && output.arn == "arn:aws:s3:::openporch-test-uploads" && output.region != ""
    error_message = "The outputs name the bucket."
  }
}

run "versioned_with_expiry" {
  variables {
    name     = "openporch-test-archive"
    resource = { versioning = true, expireAfterDays = 30 }
  }

  assert {
    condition     = aws_s3_bucket_versioning.this[0].versioning_configuration[0].status == "Enabled"
    error_message = "The bucket is versioned."
  }
  assert {
    condition     = one(aws_s3_bucket_lifecycle_configuration.this[0].rule).expiration[0].days == 30
    error_message = "Objects expire after 30 days."
  }
}

run "invalid_name" {
  command = plan

  variables {
    name = "Uploads"
  }

  expect_failures = [var.name]
}
