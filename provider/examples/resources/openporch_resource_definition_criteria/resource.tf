resource "openporch_resource_definition_criteria" "staging" {
  resource_definition_id = openporch_resource_definition.redis.id
  env_type               = "staging"

  # Changing the definition's type replaces it, and the server deletes its
  # criteria with it; this recreates them in the same apply.
  lifecycle {
    replace_triggered_by = [openporch_resource_definition.redis]
  }
}
