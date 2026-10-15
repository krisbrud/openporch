# A service user, such as the one a CI pipeline deploys as.
resource "openporch_user" "ci" {
  id   = "ci"
  name = "CI pipeline"
  role = "member"
}
