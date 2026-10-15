#!/usr/bin/env bash
# Builds the provider and checks that OpenTofu loads it and authenticates against
# a dev server, both ways a provider developer loads an unpublished build:
#
#   1. `tofu init` installs it from a local filesystem mirror, then `tofu apply`
#      reads `openporch_current_user` with the token from `OPENPORCH_TOKEN`.
#   2. A `dev_overrides` entry loads it without `tofu init`, and `tofu plan`
#      succeeds with the three `OPENPORCH_*` variables and no provider arguments.
#
# Usage: provider/scripts/smoke.sh PATH_TO_OPENPORCH_BINARY
# Needs `go` and `tofu` on PATH.
set -euo pipefail

server_bin=$(realpath "${1:?usage: smoke.sh PATH_TO_OPENPORCH_BINARY}")
provider_dir=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'kill "${server_pid:-}" 2>/dev/null || true; rm -rf "$work"' EXIT

version=0.0.1
platform="$(go env GOOS)_$(go env GOARCH)"
mirror="$work/mirror/registry.opentofu.org/openporch/openporch/$version/$platform"
mkdir -p "$mirror" "$work/dev" "$work/data" "$work/init" "$work/override"
(cd "$provider_dir" && go build -ldflags "-X main.version=$version" -o "$mirror/terraform-provider-openporch_v$version" .)
cp "$mirror/terraform-provider-openporch_v$version" "$work/dev/terraform-provider-openporch"

port=$((20000 + RANDOM % 20000))
OPENPORCH_ORG=acme OPENPORCH_DATA_DIR="$work/data" OPENPORCH_LISTEN="127.0.0.1:$port" \
  "$server_bin" serve >"$work/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 100); do
  [ -s "$work/data/bootstrap-token" ] && curl -sf "http://127.0.0.1:$port/healthz" >/dev/null && break
  sleep 0.2
done
if ! [ -s "$work/data/bootstrap-token" ]; then
  echo "the server did not start:" >&2
  cat "$work/server.log" >&2
  exit 1
fi

export OPENPORCH_URL="http://127.0.0.1:$port" OPENPORCH_ORG=acme
OPENPORCH_TOKEN=$(cat "$work/data/bootstrap-token")
export OPENPORCH_TOKEN
export TF_IN_AUTOMATION=1

config='terraform {
  required_providers {
    openporch = {
      source  = "openporch/openporch"
      version = "'"$version"'"
    }
  }
}

provider "openporch" {}

data "openporch_current_user" "me" {}

output "user" {
  value = data.openporch_current_user.me.id
}
'

echo "== tofu init from a filesystem mirror"
printf '%s' "$config" >"$work/init/main.tf"
cat >"$work/mirror.tofurc" <<RC
provider_installation {
  filesystem_mirror {
    path    = "$work/mirror"
    include = ["registry.opentofu.org/openporch/openporch"]
  }
  direct {
    exclude = ["registry.opentofu.org/openporch/openporch"]
  }
}
RC
(
  cd "$work/init"
  export TF_CLI_CONFIG_FILE="$work/mirror.tofurc"
  tofu init -no-color -input=false
  tofu apply -no-color -input=false -auto-approve
  user=$(tofu output -raw user)
  if [ "$user" != bootstrap ]; then
    echo "expected the bootstrap user, got '$user'" >&2
    exit 1
  fi
)

echo "== tofu plan through a dev override"
printf '%s' "$config" | sed '/version =/d' >"$work/override/main.tf"
cat >"$work/override.tofurc" <<RC
provider_installation {
  dev_overrides {
    "registry.opentofu.org/openporch/openporch" = "$work/dev"
  }
  direct {}
}
RC
(
  cd "$work/override"
  export TF_CLI_CONFIG_FILE="$work/override.tofurc"
  tofu plan -no-color -input=false
  echo "== a wrong token fails the plan"
  if OPENPORCH_TOKEN=opt_wrong tofu plan -no-color -input=false >"$work/wrong.log" 2>&1; then
    echo "a wrong token was accepted" >&2
    exit 1
  fi
  grep -q "AUTH-001" "$work/wrong.log"
  if grep -q "opt_wrong" "$work/wrong.log"; then
    echo "the plan output shows the token" >&2
    exit 1
  fi
)
echo "== ok"
