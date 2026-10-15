variable "app" {
  type        = string
  description = "The application ID, `$${context.app.id}`."
}

variable "env" {
  type        = string
  description = "The environment ID, `$${context.env.id}`."
}

variable "resource" {
  type        = any
  default     = {}
  nullable    = false
  description = "The workload resource's inputs (`id`, `class`, `spec`, `dependencies`), set by the OpenTofu driver."
}

variable "resource_secrets" {
  type        = any
  default     = {}
  nullable    = false
  sensitive   = true
  description = "The values of the workload's inputs that resolved from secrets, at their paths, set by the OpenTofu driver."
}

variable "log_retention_days" {
  type        = number
  default     = 14
  description = "How long the function's log group keeps logs."
}

variable "iam_path" {
  type        = string
  default     = "/"
  description = "The IAM path of the function's roles, such as `/openporch/`."

  validation {
    condition     = can(regex("^/([A-Za-z0-9+=,.@_-]+/)*$", var.iam_path))
    error_message = "An IAM path starts and ends with `/`."
  }
}

variable "permissions_boundary" {
  type        = string
  default     = null
  description = "The ARN of a managed policy that bounds the function's roles, which a platform's role may be required to set."
}

variable "tags" {
  type    = map(string)
  default = {}
}
