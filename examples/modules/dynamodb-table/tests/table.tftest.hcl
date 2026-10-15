# Runs against the AWS account or emulator the environment points at, such as
# Floci with AWS_ENDPOINT_URL=http://localhost:4566.

run "default_key" {
  command = plan

  variables {
    name = "openporch-test-default"
  }

  assert {
    condition     = aws_dynamodb_table.this.hash_key == "id" && aws_dynamodb_table.this.range_key == null
    error_message = "Without params the key is the string `id`."
  }
  assert {
    condition     = aws_dynamodb_table.this.billing_mode == "PAY_PER_REQUEST"
    error_message = "The table is on demand."
  }
}

run "key_sort_key_and_index" {
  variables {
    name = "openporch-test-orders"
    resource = {
      primaryKey = { name = "id", type = "S" }
      sortKey    = { name = "created" }
      attributes = { created = "N" }
      indexes = {
        by-customer = { primaryKey = { name = "customer", type = "S" }, sortKey = { name = "created" } }
      }
      stream = "NEW_IMAGE"
    }
  }

  assert {
    condition = (
      aws_dynamodb_table.this.hash_key == "id" &&
      aws_dynamodb_table.this.range_key == "created" &&
      length(aws_dynamodb_table.this.attribute) == 3 &&
      contains([for a in aws_dynamodb_table.this.attribute : "${a.name}:${a.type}"], "created:N")
    )
    error_message = "The table has the keys and attribute types the params give."
  }
  assert {
    condition     = one(aws_dynamodb_table.this.global_secondary_index).key_schema[0].attribute_name == "customer"
    error_message = "The index is keyed on `customer`."
  }
  assert {
    condition     = output.name == "openporch-test-orders" && startswith(output.arn, "arn:aws:dynamodb:")
    error_message = "The outputs name the table."
  }
  assert {
    condition     = output.region != "" && output.stream_arn != null
    error_message = "The outputs give the region and the stream."
  }
}

run "conflicting_key_types" {
  command = plan

  variables {
    name = "openporch-test-conflict"
    resource = {
      primaryKey = { name = "id", type = "S" }
      indexes    = { by-id = { primaryKey = { name = "id", type = "N" } } }
    }
  }

  expect_failures = [aws_dynamodb_table.this]
}

run "invalid_name" {
  command = plan

  variables {
    name = "no spaces"
  }

  expect_failures = [var.name]
}
