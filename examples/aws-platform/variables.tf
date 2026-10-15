variable "aws_region" {
  type        = string
  default     = "eu-north-1"
  description = "The region functions, tables, queues and buckets are created in."
}

variable "trusted_principal_arn" {
  type        = string
  description = "The identity the openporch server runs as, such as its instance or task role, which may assume the platform's role."
}

variable "external_id" {
  type        = string
  description = "The external ID openporch presents when it assumes the role; a long random value."

  validation {
    condition     = length(var.external_id) >= 16
    error_message = "Use an external ID of at least 16 characters."
  }
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
