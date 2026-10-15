run "standard" {
  variables {
    name = "openporch-test-inbox"
  }

  assert {
    condition     = !aws_sqs_queue.this.fifo_queue && aws_sqs_queue.this.visibility_timeout_seconds == 30
    error_message = "Without params the queue is a standard queue with a 30 second visibility timeout."
  }
  assert {
    condition     = length(aws_sqs_queue.dead_letter) == 0 && try(aws_sqs_queue.this.redrive_policy, "") == ""
    error_message = "Without `maxReceiveCount` there is no dead-letter queue."
  }
  assert {
    condition     = startswith(output.arn, "arn:aws:sqs:") && endswith(output.url, "openporch-test-inbox") && output.region != ""
    error_message = "The outputs name the queue."
  }
}

run "fifo_with_dead_letters" {
  variables {
    name = "openporch-test-orders"
    resource = {
      fifo              = true
      visibilityTimeout = 120
      maxReceiveCount   = 5
    }
  }

  assert {
    condition     = aws_sqs_queue.this.name == "openporch-test-orders.fifo" && aws_sqs_queue.dead_letter[0].name == "openporch-test-orders-dlq.fifo"
    error_message = "A FIFO queue and its dead-letter queue end in `.fifo`."
  }
  assert {
    condition     = jsondecode(aws_sqs_queue.this.redrive_policy).maxReceiveCount == 5
    error_message = "Messages move to the dead-letter queue after 5 receives."
  }
  assert {
    condition     = aws_sqs_queue.this.visibility_timeout_seconds == 120
    error_message = "The visibility timeout comes from the params."
  }
}

run "name_too_long" {
  command = plan

  variables {
    name = "a23456789012345678901234567890123456789012345678901234567890123456789012"
  }

  expect_failures = [var.name]
}

run "retention_out_of_range" {
  command = plan

  variables {
    name     = "openporch-test-bad"
    resource = { messageRetention = 10 }
  }

  expect_failures = [aws_sqs_queue.this]
}
