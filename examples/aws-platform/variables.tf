variable "aws_region" {
  type        = string
  default     = "eu-north-1"
  description = "The region functions, tables, queues and buckets are created in."
}

variable "credentials" {
  type        = string
  default     = "aws-role"
  description = "How openporch reaches AWS. `aws-role`: the server's own identity (`trusted_principal_arn`) assumes the platform's role with `external_id`. `aws-identity`: openporch presents a token from its own OIDC issuer (`oidc_issuer_url`), so neither openporch nor its host holds any AWS credential."

  validation {
    condition     = contains(["aws-role", "aws-identity"], var.credentials)
    error_message = "Use `aws-role` or `aws-identity`."
  }
}

variable "trusted_principal_arn" {
  type        = string
  default     = null
  description = "With `aws-role`: the identity the openporch server runs as, such as its instance or task role, which may assume the platform's role."
}

variable "external_id" {
  type        = string
  default     = null
  description = "With `aws-role`: the external ID openporch presents when it assumes the role; a long random value."

  validation {
    condition     = var.external_id == null || try(length(var.external_id) >= 16, false)
    error_message = "Use an external ID of at least 16 characters."
  }
}

variable "oidc_issuer_url" {
  type        = string
  default     = null
  description = "With `aws-identity`: the openporch issuer, `<public_url>/oidc`, such as `https://porch.example.com/oidc`."

  validation {
    condition     = var.oidc_issuer_url == null || can(regex("^https://[^/?#]+/oidc$", var.oidc_issuer_url))
    error_message = "The issuer is `https://<host>/oidc`: the server's `public_url` followed by `/oidc`."
  }
}

variable "org" {
  type        = string
  default     = null
  description = "With `aws-identity`: the openporch organization, the first half of the subject `<org>/aws` the role trusts."
}

variable "create_oidc_provider" {
  type        = bool
  default     = true
  description = "With `aws-identity`: create the IAM OIDC identity provider for the issuer. Set false when the AWS account already has one for it, such as from another platform."
}

variable "name_prefix" {
  type        = string
  default     = "openporch"
  description = "Prefixes the role, the artifact bucket and the buckets workloads ask for, which must be unique across AWS."

  validation {
    condition     = can(regex("^[a-z0-9][a-z0-9-]{1,20}$", var.name_prefix))
    error_message = "The prefix is 2 to 21 lowercase letters, digits and `-`."
  }
}

variable "artifact_noncurrent_days" {
  type        = number
  default     = null
  description = "Unset, the artifact bucket keeps every version, so every deployment can be rolled back to and replayed. Set, a version is deleted this many days after a newer one replaces it or it is deleted, and deployments that ran it can no longer be replayed."

  validation {
    condition     = var.artifact_noncurrent_days == null || try(var.artifact_noncurrent_days >= 1, false)
    error_message = "Keep replaced versions for at least a day, or leave this unset to keep them."
  }
}

variable "modules_source" {
  type        = string
  default     = "https://github.com/krisbrud/openporch"
  description = "The Git repository with the modules under `examples/modules`."
}

variable "modules_rev" {
  type        = string
  default     = "main"
  description = "The branch, tag or commit of `modules_source`; pin a tag or commit in a real platform."
}

variable "dns_zone" {
  type        = string
  default     = "apps.example.com"
  description = "The zone `dns` resources get names in, as `<app>-<env>.<zone>`. The echo driver only reports the name; it creates no record."
}
