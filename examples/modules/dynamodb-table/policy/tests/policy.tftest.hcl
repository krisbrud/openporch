run "read_write" {
  variables {
    name      = "openporch-test-orders-access"
    table_arn = "arn:aws:dynamodb:us-east-1:000000000000:table/orders"
  }

  assert {
    condition = (
      contains(jsondecode(output.document).Statement[0].Action, "dynamodb:PutItem") &&
      contains(jsondecode(output.document).Statement[0].Resource, "arn:aws:dynamodb:us-east-1:000000000000:table/orders/index/*")
    )
    error_message = "The policy writes the table and reads its indexes."
  }
  assert {
    condition     = startswith(output.arn, "arn:aws:iam::")
    error_message = "The output is the policy's ARN."
  }
}

run "read_only" {
  command = plan

  variables {
    name      = "openporch-test-orders-read"
    table_arn = "arn:aws:dynamodb:us-east-1:000000000000:table/orders"
    read_only = true
  }

  assert {
    condition     = !contains(jsondecode(data.aws_iam_policy_document.this.json).Statement[0].Action, "dynamodb:PutItem")
    error_message = "A read-only policy does not write."
  }
}

run "not_a_table" {
  command = plan

  variables {
    name      = "openporch-test-wrong"
    table_arn = "arn:aws:sqs:us-east-1:000000000000:orders"
  }

  expect_failures = [var.table_arn]
}
