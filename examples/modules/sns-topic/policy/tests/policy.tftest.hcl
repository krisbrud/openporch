run "publish" {
  variables {
    name      = "openporch-test-events-access"
    topic_arn = "arn:aws:sns:us-east-1:000000000000:events"
  }

  assert {
    condition = (
      contains(jsondecode(output.document).Statement[0].Action, "sns:Publish") &&
      !contains(jsondecode(output.document).Statement[0].Action, "sns:Subscribe") &&
      jsondecode(output.document).Statement[0].Resource == "arn:aws:sns:us-east-1:000000000000:events"
    )
    error_message = "By default the policy publishes to the one topic and nothing else."
  }
  assert {
    condition     = startswith(output.arn, "arn:aws:iam::")
    error_message = "The output is the policy's ARN."
  }
}

run "subscribe_only" {
  command = plan

  variables {
    name      = "openporch-test-events-subscribe"
    topic_arn = "arn:aws:sns:us-east-1:000000000000:orders.fifo"
    access    = "subscribe"
  }

  assert {
    condition = (
      contains(jsondecode(data.aws_iam_policy_document.this.json).Statement[0].Action, "sns:Subscribe") &&
      !contains(jsondecode(data.aws_iam_policy_document.this.json).Statement[0].Action, "sns:Publish")
    )
    error_message = "A subscribe-only policy does not publish."
  }
}

run "not_a_topic" {
  command = plan

  variables {
    name      = "openporch-test-bad"
    topic_arn = "arn:aws:sqs:us-east-1:000000000000:inbox"
  }

  expect_failures = [var.topic_arn]
}
