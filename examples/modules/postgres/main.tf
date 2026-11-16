# A `postgres` resource: an RDS for PostgreSQL instance with one database,
# in subnets and security groups the platform provides.
#
# RDS is not pay-per-use: the smallest instance runs, and is billed, all the
# time. Functions reach it from inside the VPC, so the platform puts them in
# the same VPC (see `subnet_ids` in `aws-lambda-workload`).
#
# Settings come from the resource's inputs, the `params` a workload gives the
# resource, which the OpenTofu driver passes as `var.resource`:
#
#   resources:
#     db:
#       type: postgres
#       params:
#         instanceClass: db.t4g.micro   # the default
#         storage: 20                   # GiB, the default; it grows up to `maxStorage`
#         database: orders              # the database's name; `app` by default
#
# `engineVersion` (default `17`), `maxStorage` (default 100), `backupRetention`
# (days, default 7) and `multiAz` (default false) are also read.
#
# The master password is generated and is a secret output; it is also in the
# OpenTofu state, which the driver keeps. Deleting the resource takes a final
# snapshot named `<name>-final-<suffix>`, which is kept, unless
# `finalSnapshot: false`.

terraform {
  required_version = ">= 1.8"
  required_providers {
    aws    = { source = "hashicorp/aws", version = "~> 6.0" }
    random = { source = "hashicorp/random", version = "~> 3.6" }
  }
}

variable "name" {
  type        = string
  description = "The instance identifier, unique in the account and region."

  validation {
    condition     = can(regex("^[a-z][a-z0-9-]{0,62}$", var.name)) && !endswith(var.name, "-") && !strcontains(var.name, "--")
    error_message = "An instance identifier starts with a letter and has up to 63 lowercase letters, digits and single `-`, not at the end."
  }
}

variable "resource" {
  type        = any
  default     = {}
  nullable    = false
  description = "The resource's inputs, set by the OpenTofu driver."
}

variable "subnet_group" {
  type        = string
  description = "The DB subnet group the instance is placed in, from the platform."
}

variable "security_group_ids" {
  type        = list(string)
  description = "The security groups of the instance, from the platform; one of them lets the functions' group connect on port 5432."

  validation {
    condition     = length(var.security_group_ids) > 0
    error_message = "Give at least one security group."
  }
}

variable "tags" {
  type    = map(string)
  default = {}
}

locals {
  params         = var.resource
  instance_class = try(tostring(local.params.instanceClass), "db.t4g.micro")
  storage        = try(tonumber(local.params.storage), 20)
  max_storage    = try(tonumber(local.params.maxStorage), 100)
  database       = try(tostring(local.params.database), "app")
  engine_version = try(tostring(local.params.engineVersion), "17")
  retention      = try(tonumber(local.params.backupRetention), 7)
  multi_az       = try(tobool(local.params.multiAz), false)
  final_snapshot = try(tobool(local.params.finalSnapshot), true)
}

resource "random_password" "master" {
  length = 32
  # Characters RDS refuses in a master password: `/`, `"`, `@` and space.
  override_special = "!#$%&*()-_=+[]{}<>:?"
}

# Gives each final snapshot its own name, so a resource deleted and created
# again does not collide with the snapshot of the one before.
resource "random_id" "snapshot" {
  byte_length = 4
}

resource "aws_db_instance" "this" {
  identifier     = var.name
  engine         = "postgres"
  engine_version = local.engine_version
  instance_class = local.instance_class

  allocated_storage     = local.storage
  max_allocated_storage = local.max_storage
  storage_type          = "gp3"
  storage_encrypted     = true

  db_name  = local.database
  username = "openporch"
  password = random_password.master.result

  db_subnet_group_name   = var.subnet_group
  vpc_security_group_ids = var.security_group_ids
  publicly_accessible    = false
  multi_az               = local.multi_az

  backup_retention_period    = local.retention
  auto_minor_version_upgrade = true
  copy_tags_to_snapshot      = true
  skip_final_snapshot        = !local.final_snapshot
  final_snapshot_identifier  = local.final_snapshot ? "${var.name}-final-${random_id.snapshot.hex}" : null
  # openporch deletes the instance only with an acknowledged deletion.
  deletion_protection = false
  apply_immediately   = true

  tags = var.tags

  lifecycle {
    precondition {
      condition     = can(regex("^[A-Za-z][A-Za-z0-9_]{0,62}$", local.database))
      error_message = "`database` starts with a letter and has up to 63 letters, digits and `_`."
    }
    precondition {
      condition     = local.storage >= 20 && local.max_storage >= local.storage
      error_message = "`storage` is at least 20 GiB, and `maxStorage` at least `storage`."
    }
    precondition {
      condition     = local.retention >= 0 && local.retention <= 35
      error_message = "`backupRetention` is 0 to 35 days."
    }
  }
}

output "host" {
  value = aws_db_instance.this.address
}

output "port" {
  value = aws_db_instance.this.port
}

output "name" {
  value = aws_db_instance.this.db_name
}

output "username" {
  value     = aws_db_instance.this.username
  sensitive = true
}

output "password" {
  value     = random_password.master.result
  sensitive = true
}
