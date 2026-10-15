# A role openporch assumes.
resource "openporch_cloud_account" "aws" {
  id   = "aws-prod"
  type = "aws-role"
  config = {
    role_arn    = "arn:aws:iam::123456789012:role/openporch"
    external_id = "acme"
    region      = "eu-north-1"
  }
}

# A static key pair; the secret key is written to the built-in store.
resource "openporch_cloud_account" "aws_dev" {
  id   = "aws-dev"
  type = "aws"
  config = {
    access_key_id = "AKIAIOSFODNN7EXAMPLE"
    region        = "eu-north-1"
  }
  secrets = {
    secret_access_key = var.aws_secret_access_key
  }
}
