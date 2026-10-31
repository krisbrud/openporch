# What the account holds after the workload's removal.

terraform {
  required_providers {
    aws = { source = "hashicorp/aws", version = ">= 6.28, < 7.0" }
  }
}

variable "queue" {
  type = string
}

variable "queue_arn" {
  type = string
}

data "aws_lambda_functions" "all" {}

data "aws_sqs_queue" "queue" {
  name = var.queue
}
