terraform {
  required_providers {
  }
}

module "main" {
  source = "./module"

  env = {
    NORMAL_KEY = "value3"
    "with space" = "value2"
    with-hyphen = "value1"
  }
}

