run "read_write" {
  variables {
    name       = "openporch-test-uploads-access"
    bucket_arn = "arn:aws:s3:::openporch-test-uploads"
  }

  assert {
    condition = (
      jsondecode(output.document).Statement[0].Resource == "arn:aws:s3:::openporch-test-uploads" &&
      jsondecode(output.document).Statement[1].Resource == "arn:aws:s3:::openporch-test-uploads/*" &&
      contains(jsondecode(output.document).Statement[1].Action, "s3:PutObject")
    )
    error_message = "The policy lists the bucket and writes its objects."
  }
  assert {
    condition     = startswith(output.arn, "arn:aws:iam::")
    error_message = "The output is the policy's ARN."
  }
}

run "read_only" {
  command = plan

  variables {
    name       = "openporch-test-uploads-read"
    bucket_arn = "arn:aws:s3:::openporch-test-uploads"
    read_only  = true
  }

  assert {
    condition     = jsondecode(data.aws_iam_policy_document.this.json).Statement[1].Action == "s3:GetObject"
    error_message = "A read-only policy only reads objects."
  }
}

run "not_a_bucket" {
  command = plan

  variables {
    name       = "openporch-test-wrong"
    bucket_arn = "arn:aws:sqs:us-east-1:000000000000:inbox"
  }

  expect_failures = [var.bucket_arn]
}
