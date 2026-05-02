terraform {
  required_providers {
    docker = {
      source = "kreuzwerker/docker"
    }
  }
}

variable "name" {
  type = string
}
variable "image" {
  type = string
}
variable "host_port" {
  type    = number
  default = 8080
}
variable "container_port" {
  type    = number
  default = 8080
}
variable "env" {
  type    = map(string)
  default = {}
}

resource "docker_image" "this" {
  name         = var.image
  keep_locally = true
}

resource "docker_container" "this" {
  name  = "openporch-${var.name}"
  image = docker_image.this.image_id
  env   = [for k, v in var.env : "${k}=${v}"]
  ports {
    internal = var.container_port
    external = var.host_port
    ip       = "0.0.0.0"
  }
  restart = "unless-stopped"
  labels {
    label = "openporch.managed"
    value = "true"
  }
}

output "url" { value = "http://localhost:${var.host_port}" }
output "name" { value = docker_container.this.name }
