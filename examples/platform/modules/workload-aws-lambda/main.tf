variable "image" {
  type = string
}
variable "env" {
  type    = map(string)
  default = {}
}
variable "name" {
  type = string
}
output "url" { value = "https://lambda-${var.name}.execute-api.us-east-1.amazonaws.com/prod" }
output "name" { value = "lambda-${var.name}" }
