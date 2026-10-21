#!/usr/bin/env bash
# Regenerates the provider documentation in docs/ with tfplugindocs, from the
# schema's descriptions and the configurations in examples/, after checking that
# the examples are formatted and valid together.
#
# tfplugindocs looks for the schema under a registry.terraform.io/hashicorp
# address and installs the provider with `terraform init`, so this script exports
# the schema with OpenTofu through a `dev_overrides` entry instead, and hands it
# to tfplugindocs under the short name `openporch`.
#
# Usage: provider/scripts/docs.sh
# Needs `go`, `tofu` and `jq` on PATH. CI uses OpenTofu 1.11.5; another version
# may export a slightly different schema, which shows up as a diff in docs/.
set -euo pipefail

provider_dir=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/dev" "$work/config"

(cd "$provider_dir" && go build -o "$work/dev/terraform-provider-openporch" .)
cat >"$work/tofurc" <<RC
provider_installation {
  dev_overrides {
    "openporch/openporch" = "$work/dev"
  }
  direct {}
}
RC
cat >"$work/config/main.tf" <<'TF'
terraform {
  required_providers {
    openporch = {
      source = "openporch/openporch"
    }
  }
}
TF
export TF_CLI_CONFIG_FILE="$work/tofurc" TF_IN_AUTOMATION=1
tofu fmt -check -recursive "$provider_dir/examples"
# The examples form one configuration: each refers to objects of the others.
mkdir "$work/examples"
n=0
while IFS= read -r -d '' f; do
  n=$((n + 1))
  cp "$f" "$work/examples/$n.tf"
done < <(find "$provider_dir/examples" -name '*.tf' -print0)
cat >"$work/examples/variables.tf" <<'TF'
variable "payment_api_key" { sensitive = true }
variable "aws_secret_access_key" { sensitive = true }
variable "redis_auth_token" { sensitive = true }
variable "driver_token" { sensitive = true }
TF
(cd "$work/examples" && tofu validate -no-color)

(cd "$work/config" && TF_CLI_CONFIG_FILE="$work/tofurc" tofu providers schema -json) |
  jq '.provider_schemas |= with_entries(select(.key == "registry.opentofu.org/openporch/openporch") | .key = "openporch")' \
    >"$work/schema.json"

cd "$provider_dir"
go tool tfplugindocs generate --provider-name openporch --providers-schema "$work/schema.json"
