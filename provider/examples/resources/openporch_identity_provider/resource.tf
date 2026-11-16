variable "corp_client_secret" {
  type      = string
  sensitive = true
}

# People sign in at https://porch.example.com/auth/oidc/corp/authorize.
resource "openporch_identity_provider" "corp" {
  id            = "corp"
  issuer        = "https://login.corp.example"
  client_id     = "openporch"
  client_secret = var.corp_client_secret
  callback_url  = "https://porch.example.com/auth/oidc/corp/callback"
  return_urls   = ["https://porch.example.com/"]

  # Lets `porch login --provider corp` finish on the loopback address.
  loopback_return = true
}
