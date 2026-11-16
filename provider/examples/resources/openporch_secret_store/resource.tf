resource "openporch_secret_store" "vault" {
  id            = "vault"
  type          = "aws-secrets-manager"
  region        = "eu-north-1"
  cloud_account = openporch_cloud_account.aws.id
  primary       = true
}
