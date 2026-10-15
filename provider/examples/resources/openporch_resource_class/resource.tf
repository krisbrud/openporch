resource "openporch_resource_class" "large" {
  resource_type = "postgres"
  id            = "large"
  description   = "Databases with more memory"
}
