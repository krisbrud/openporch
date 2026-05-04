terraform {
  required_providers {
    docker = {
      source = "kreuzwerker/docker"
      version = "~> 3.0"
    }
  }
}

provider "docker" {
  alias = "default"
  host = "unix:///var/run/docker.sock"
}

module "main" {
  source = "./module"

  providers = {
    docker = docker.default
  }

  env = {
    DATABASE_URL = "postgres://x"
    PORT = 8080
  }
  image = "myorg/api:1.0"
}

output "host" {
  value = module.main.host
}

output "url" {
  value = module.main.url
}

