# openporch placeholders are written $${...} so OpenTofu does not interpolate
# them.
resource "openporch_resource_definition" "redis" {
  id             = "redis"
  type           = "redis"
  driver_type    = "openporch/opentofu"
  driver_account = openporch_cloud_account.aws.id
  driver_inputs = {
    source    = { url = "https://github.com/acme/modules", path = "redis" }
    variables = { name = "$${context.app.id}-$${context.env.id}" }
  }
  driver_secrets = {
    variables = {
      # Written to the built-in store.
      auth_token = var.redis_auth_token
      # Kept as a template and resolved at deployment.
      upstream_password = "$${resources.postgres.outputs.password}"
    }
  }
}
