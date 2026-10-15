resource "openporch_value" "log_level" {
  app_id = openporch_application.shop.id
  key    = "LOG_LEVEL"
  value  = "info"
}

# An environment value overrides the application value with the same key.
resource "openporch_value" "log_level_staging" {
  app_id = openporch_value.log_level.app_id
  env_id = openporch_environment.staging.id
  key    = openporch_value.log_level.key
  value  = "debug"
}

# A secret value is written to the built-in store and tracked by version.
resource "openporch_value" "api_key" {
  app_id       = openporch_application.shop.id
  key          = "PAYMENT_API_KEY"
  secret_value = var.payment_api_key
}
