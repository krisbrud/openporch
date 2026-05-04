terraform {
  required_providers {
    aws = {
      source = "hashicorp/aws"
      version = "~> 5.0"
    }
    docker = {
      source = "kreuzwerker/docker"
      version = "~> 3.0"
    }
  }
}

provider "aws" {
  alias = "us_east_1"
  region = "us-east-1"
}

provider "docker" {
  alias = "default"
  host = "unix:///var/run/docker.sock"
}

module "main" {
  source = "./mod"

  providers = {
    aws = aws.us_east_1
    docker = docker.default
  }
}

output "endpoint" {
  value = module.main.endpoint
}

