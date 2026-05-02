terraform {
  required_providers {
    docker = {
      source = "kreuzwerker/docker"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.6"
    }
  }
}

variable "size" {
  type    = string
  default = "small"
}
variable "res_id" {
  type = string
}
variable "host_port" {
  type    = number
  default = 5433
}

resource "random_password" "this" {
  length  = 16
  special = false
}

resource "docker_image" "postgres" {
  name         = "postgres:16-alpine"
  keep_locally = true
}

resource "docker_container" "this" {
  name  = "openporch-${var.res_id}"
  image = docker_image.postgres.image_id
  env = [
    "POSTGRES_USER=demo",
    "POSTGRES_PASSWORD=${random_password.this.result}",
    "POSTGRES_DB=${var.res_id}",
  ]
  ports {
    internal = 5432
    external = var.host_port
    ip       = "0.0.0.0"
  }
  healthcheck {
    test     = ["CMD-SHELL", "pg_isready -U demo -d ${var.res_id}"]
    interval = "2s"
    timeout  = "2s"
    retries  = 30
  }
  wait         = true
  wait_timeout = 60
  restart      = "unless-stopped"
  labels {
    label = "openporch.managed"
    value = "true"
  }
}

# url contains the random password. v0 doesn't propagate `sensitive` from
# child outputs to the root module, so we use nonsensitive() to let the
# value flow through to the orchestrator. Treat outputs.json on disk as
# secret-equivalent.
output "url" {
  value = "postgres://demo:${nonsensitive(random_password.this.result)}@host.docker.internal:${var.host_port}/${var.res_id}"
}
output "host" {
  value = "host.docker.internal"
}
output "port" {
  value = var.host_port
}
