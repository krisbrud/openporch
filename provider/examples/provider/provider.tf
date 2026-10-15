terraform {
  required_providers {
    openporch = {
      source = "openporch/openporch"
    }
  }
}

# Each argument may be left out and taken from OPENPORCH_URL, OPENPORCH_ORG and
# OPENPORCH_TOKEN instead.
provider "openporch" {
  url    = "https://porch.example.com"
  org_id = "acme"
  token  = var.openporch_token
}

variable "openporch_token" {
  type      = string
  sensitive = true
}
