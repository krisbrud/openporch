resource "openporch_application" "shop" {
  id   = "shop"
  name = "Web shop"

  # Deleting the application deletes the cloud resources it owns; list them
  # here, as the deletion error prints them (with @FINGERPRINT when it shows
  # one), before removing it.
  destroy_acknowledgements = [
    "development:postgres.default#modules.web.externals.db=delete",
  ]
}
