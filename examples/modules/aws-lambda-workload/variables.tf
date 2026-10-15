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

variable "tags" {
  type    = map(string)
  default = {}
}
