# To rotate the token, change expires_at together with id.
resource "openporch_service_user_token" "ci" {
  user_id     = openporch_user.ci.id
  id          = "ci-2027"
  description = "GitHub Actions"
  expires_at  = "2027-01-01T00:00:00Z"
}
