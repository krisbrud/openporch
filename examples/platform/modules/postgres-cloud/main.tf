variable "size" {
  type    = string
  default = "small"
}
variable "res_id" {
  type    = string
  default = "db"
}
locals {
  cluster = "aurora-${var.res_id}"
  region  = "us-east-1"
}
output "url" { value = "postgres://appuser:CHANGE_ME@${local.cluster}.${local.region}.rds.amazonaws.com:5432/main" }
output "host" { value = "${local.cluster}.${local.region}.rds.amazonaws.com" }
output "port" { value = 5432 }
