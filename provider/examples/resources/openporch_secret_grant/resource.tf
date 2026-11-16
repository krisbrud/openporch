# Lets production of the shop use a secret someone else put in the store.
resource "openporch_secret_grant" "stripe" {
  store  = openporch_secret_store.vault.id
  ref    = "shop/stripe-key"
  app_id = openporch_application.shop.id
  env_id = "production"
}
