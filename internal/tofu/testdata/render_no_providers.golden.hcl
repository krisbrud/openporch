terraform {
  required_providers {
  }
}

module "main" {
  source = "./module"

  name = "api"
}

output "url" {
  value = module.main.url
}

