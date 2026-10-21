# An external HTTP driver; the token is written to the built-in store. Changing
# the token or the target updates the registration in place.
resource "openporch_driver" "vault_db" {
  id     = "acme/vault-db"
  target = "https://drivers.acme.example/vault-db"
  token  = var.driver_token
  capabilities = {
    observe = true
  }
}

resource "openporch_resource_definition" "vault_db" {
  id          = "vault-db"
  type        = "postgres"
  driver_type = openporch_driver.vault_db.id
}
