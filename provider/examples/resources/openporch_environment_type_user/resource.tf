# Lets the service user deploy to every environment of the type.
resource "openporch_environment_type_user" "ci" {
  env_type_id = openporch_environment_type.staging.id
  user_id     = openporch_user.ci.id
  role        = "deployer"
}
