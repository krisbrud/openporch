resource "openporch_resource_type" "feature_flags" {
  id       = "acme/feature-flags"
  name     = "Feature flags"
  use      = "direct"
  category = "config"
  inputs_schema = {
    type       = "object"
    properties = { project = { type = "string" } }
  }
  outputs_schema = {
    type = "object"
    properties = {
      values  = { type = "object", properties = { url = { type = "string" } } }
      secrets = { type = "object", properties = { sdk_key = { type = "string" } } }
    }
  }
}
