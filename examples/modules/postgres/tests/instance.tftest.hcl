# Applies the module with a mocked AWS provider, since the emulator CI uses has
# no RDS: how it reads the params, and what it refuses.

mock_provider "aws" {
  mock_resource "aws_db_instance" {
    defaults = {
      address = "openporch-test-db.abc123.eu-north-1.rds.amazonaws.com"
      port    = 5432
    }
  }
}

variables {
  name               = "openporch-test-db"
  subnet_group       = "openporch-databases"
  security_group_ids = ["sg-0123456789abcdef0"]
}

run "defaults" {
  assert {
    condition = (
      aws_db_instance.this.instance_class == "db.t4g.micro" &&
      aws_db_instance.this.allocated_storage == 20 &&
      aws_db_instance.this.engine == "postgres" &&
      aws_db_instance.this.engine_version == "17"
    )
    error_message = "Without params the instance is the smallest PostgreSQL 17 instance."
  }
  assert {
    condition = (
      aws_db_instance.this.storage_encrypted &&
      !aws_db_instance.this.publicly_accessible &&
      aws_db_instance.this.db_subnet_group_name == "openporch-databases" &&
      aws_db_instance.this.vpc_security_group_ids == toset(["sg-0123456789abcdef0"])
    )
    error_message = "The instance is encrypted, private, and in the platform's subnets and groups."
  }
  assert {
    condition = (
      !aws_db_instance.this.skip_final_snapshot &&
      startswith(aws_db_instance.this.final_snapshot_identifier, "openporch-test-db-final-")
    )
    error_message = "Deleting the instance takes a final snapshot."
  }
  assert {
    condition = (
      output.host == "openporch-test-db.abc123.eu-north-1.rds.amazonaws.com" &&
      output.port == 5432 && output.name == "app" &&
      nonsensitive(output.username) == "openporch" && length(nonsensitive(output.password)) == 32
    )
    error_message = "The outputs are the address, port, database and generated credentials."
  }
}

run "params" {
  variables {
    resource = {
      instanceClass   = "db.t4g.small"
      storage         = 50
      database        = "orders"
      multiAz         = true
      backupRetention = 14
      finalSnapshot   = false
    }
  }

  assert {
    condition = (
      aws_db_instance.this.instance_class == "db.t4g.small" &&
      aws_db_instance.this.allocated_storage == 50 &&
      aws_db_instance.this.db_name == "orders" &&
      aws_db_instance.this.multi_az &&
      aws_db_instance.this.backup_retention_period == 14
    )
    error_message = "The params set the instance."
  }
  assert {
    condition     = aws_db_instance.this.skip_final_snapshot && aws_db_instance.this.final_snapshot_identifier == null
    error_message = "`finalSnapshot: false` deletes without a snapshot."
  }
}

run "bad_database_name" {
  command = plan

  variables {
    resource = { database = "my-db" }
  }

  expect_failures = [aws_db_instance.this]
}

run "too_little_storage" {
  command = plan

  variables {
    resource = { storage = 10 }
  }

  expect_failures = [aws_db_instance.this]
}

run "bad_identifier" {
  command = plan

  variables {
    name = "Openporch_DB"
  }

  expect_failures = [var.name]
}

run "no_security_group" {
  command = plan

  variables {
    security_group_ids = []
  }

  expect_failures = [var.security_group_ids]
}
