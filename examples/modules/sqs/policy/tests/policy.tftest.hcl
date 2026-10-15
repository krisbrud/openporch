run "send_and_consume" {
  variables {
    name      = "openporch-test-inbox-access"
    queue_arn = "arn:aws:sqs:us-east-1:000000000000:inbox"
  }

  assert {
    condition = (
      contains(jsondecode(output.document).Statement[0].Action, "sqs:ReceiveMessage") &&
      contains(jsondecode(output.document).Statement[0].Action, "sqs:SendMessage") &&
      jsondecode(output.document).Statement[0].Resource == "arn:aws:sqs:us-east-1:000000000000:inbox"
    )
    error_message = "The policy sends to and consumes the one queue."
  }
  assert {
    condition     = startswith(output.arn, "arn:aws:iam::")
    error_message = "The output is the policy's ARN."
  }
}

run "send_only" {
  command = plan

  variables {
    name      = "openporch-test-inbox-send"
    queue_arn = "arn:aws:sqs:us-east-1:000000000000:inbox"
    access    = "send"
  }

  assert {
    condition = (
      contains(jsondecode(data.aws_iam_policy_document.this.json).Statement[0].Action, "sqs:SendMessage") &&
      !contains(jsondecode(data.aws_iam_policy_document.this.json).Statement[0].Action, "sqs:ReceiveMessage")
    )
    error_message = "A send-only policy does not consume."
  }
}

run "consume_only" {
  command = plan

  variables {
    name      = "openporch-test-inbox-consume"
    queue_arn = "arn:aws:sqs:us-east-1:000000000000:inbox"
    access    = "consume"
  }

  assert {
    condition = (
      contains(jsondecode(data.aws_iam_policy_document.this.json).Statement[0].Action, "sqs:DeleteMessage") &&
      !contains(jsondecode(data.aws_iam_policy_document.this.json).Statement[0].Action, "sqs:SendMessage")
    )
    error_message = "A consume-only policy does not send."
  }
}

run "not_a_queue" {
  command = plan

  variables {
    name      = "openporch-test-wrong"
    queue_arn = "arn:aws:s3:::inbox"
  }

  expect_failures = [var.queue_arn]
}
