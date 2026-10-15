resource "openporch_application_user" "ci" {
  app_id  = openporch_application.shop.id
  user_id = openporch_user.ci.id
  role    = "developer"
}
