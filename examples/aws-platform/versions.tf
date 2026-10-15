terraform {
  required_version = ">= 1.8"
  required_providers {
    openporch = {
      source = "openporch/openporch"
    }
    aws = {
      source  = "hashicorp/aws"
      version = ">= 6.0, < 7.0"
    }
  }
}

# Both providers take their settings from the environment: OPENPORCH_URL,
# OPENPORCH_ORG and OPENPORCH_TOKEN, and the standard AWS chain for the account
# the platform's bucket and role are created in.
provider "openporch" {}

provider "aws" {
  region = var.aws_region
}
