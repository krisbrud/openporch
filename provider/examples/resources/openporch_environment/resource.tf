resource "openporch_environment" "staging" {
  app_id = openporch_application.shop.id
  id     = "staging"
  type   = openporch_environment_type.staging.id
}
