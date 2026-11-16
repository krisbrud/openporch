# PostgreSQL databases, when `database` names a VPC: workloads of the
# `aws-lambda-vpc` class run in its subnets, and `postgres` resources are RDS
# instances there that only those functions can reach. RDS is billed while it
# runs, unlike the rest of the platform.
#
# Functions in the VPC reach the internet and AWS APIs only through the VPC's
# NAT gateway or endpoints, which the VPC must provide for the dependencies
# they use.

locals {
  databases = var.database != null
  # Instances and their final snapshots are named `<prefix>-<guresid>...`.
  db_names = "${var.name_prefix}-*"
}

resource "aws_db_subnet_group" "databases" {
  count       = local.databases ? 1 : 0
  name        = "${var.name_prefix}-databases"
  description = "Where openporch's postgres resources run"
  subnet_ids  = var.database.subnet_ids
}

# The functions' group: outbound only.
resource "aws_security_group" "functions" {
  count       = local.databases ? 1 : 0
  name        = "${var.name_prefix}-functions"
  description = "Functions openporch runs in the VPC"
  vpc_id      = var.database.vpc_id
}

resource "aws_vpc_security_group_egress_rule" "functions" {
  count             = local.databases ? 1 : 0
  security_group_id = aws_security_group.functions[0].id
  ip_protocol       = "-1"
  cidr_ipv4         = "0.0.0.0/0"
  description       = "Anything the functions call"
}

# The databases' group: PostgreSQL from the functions' group alone.
resource "aws_security_group" "databases" {
  count       = local.databases ? 1 : 0
  name        = "${var.name_prefix}-databases"
  description = "PostgreSQL instances openporch creates"
  vpc_id      = var.database.vpc_id
}

resource "aws_vpc_security_group_ingress_rule" "databases" {
  count                        = local.databases ? 1 : 0
  security_group_id            = aws_security_group.databases[0].id
  referenced_security_group_id = aws_security_group.functions[0].id
  ip_protocol                  = "tcp"
  from_port                    = 5432
  to_port                      = 5432
  description                  = "PostgreSQL from openporch's functions"
}

# What the platform role needs for databases and functions in the VPC.
data "aws_iam_policy_document" "databases" {
  count = local.databases ? 1 : 0
  statement {
    sid     = "Databases"
    actions = ["rds:*"]
    resources = [
      "${local.arn}:rds:*:${local.account}:db:${local.db_names}",
      "${local.arn}:rds:*:${local.account}:snapshot:${local.db_names}",
    ]
  }
  statement {
    # Creating an instance also names its subnet group and the default
    # parameter and option groups.
    sid     = "CreateDatabases"
    actions = ["rds:CreateDBInstance"]
    resources = [
      aws_db_subnet_group.databases[0].arn,
      "${local.arn}:rds:*:${local.account}:pg:default.*",
      "${local.arn}:rds:*:${local.account}:og:default:*",
    ]
  }
  statement {
    # Lambda and RDS check the subnets and groups they are given.
    sid       = "DescribeNetwork"
    actions   = ["ec2:DescribeSecurityGroups", "ec2:DescribeSubnets", "ec2:DescribeVpcs", "rds:DescribeDBEngineVersions"]
    resources = ["*"]
  }
}

resource "aws_iam_role_policy" "databases" {
  count  = local.databases ? 1 : 0
  name   = "openporch-databases"
  role   = aws_iam_role.openporch.id
  policy = data.aws_iam_policy_document.databases[0].json
}
