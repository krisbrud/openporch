run "standard" {
  variables {
    name = "openporch-test-events"
  }

  assert {
    condition     = !aws_sns_topic.this.fifo_topic && aws_sns_topic.this.name == "openporch-test-events"
    error_message = "Without params the topic is a standard topic named after the resource."
  }
  assert {
    condition     = startswith(output.arn, "arn:aws:sns:") && endswith(output.arn, ":openporch-test-events") && output.region != ""
    error_message = "The outputs name the topic."
  }
}

run "fifo" {
  variables {
    name = "openporch-test-orders"
    resource = {
      fifo                      = true
      contentBasedDeduplication = true
    }
  }

  assert {
    condition     = aws_sns_topic.this.fifo_topic && output.name == "openporch-test-orders.fifo"
    error_message = "A FIFO topic ends in `.fifo`."
  }
  assert {
    condition     = aws_sns_topic.this.content_based_deduplication
    error_message = "Content-based deduplication comes from the params."
  }
}

run "name_too_long" {
  command = plan

  variables {
    name = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  }

  expect_failures = [var.name]
}

run "deduplication_needs_fifo" {
  command = plan

  variables {
    name     = "openporch-test-bad"
    resource = { contentBasedDeduplication = true }
  }

  expect_failures = [aws_sns_topic.this]
}
