data "openporch_current_user" "me" {}

output "user" {
  value = data.openporch_current_user.me.id
}
