#!/usr/bin/env bash
# Runs the provider's acceptance tests with OpenTofu against a dev server.
#
# Usage: provider/scripts/acceptance.sh PATH_TO_OPENPORCH_BINARY [go test args...]
# Needs `go` and `tofu` on PATH.
set -euo pipefail

server_bin=$(realpath "${1:?usage: acceptance.sh PATH_TO_OPENPORCH_BINARY [go test args...]}")
shift
provider_dir=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'kill "${server_pid:-}" 2>/dev/null || true; rm -rf "$work"' EXIT

# Below Linux's ephemeral port range, so the port is rarely taken.
port=$((20000 + RANDOM % 12000))
OPENPORCH_ORG=acme OPENPORCH_DATA_DIR="$work/data" OPENPORCH_LISTEN="127.0.0.1:$port" \
  "$server_bin" serve >"$work/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 100); do
  curl -sf "http://127.0.0.1:$port/healthz" >/dev/null && break
  kill -0 "$server_pid" 2>/dev/null || break
  sleep 0.2
done
if ! curl -sf "http://127.0.0.1:$port/healthz" >/dev/null; then
  echo "the server did not start:" >&2
  cat "$work/server.log" >&2
  exit 1
fi

export OPENPORCH_URL="http://127.0.0.1:$port" OPENPORCH_ORG=acme
OPENPORCH_TOKEN=$(cat "$work/data/bootstrap-token")
export OPENPORCH_TOKEN
export TF_ACC=1
TF_ACC_TERRAFORM_PATH=$(command -v tofu)
export TF_ACC_TERRAFORM_PATH
export TF_ACC_PROVIDER_NAMESPACE=openporch TF_ACC_PROVIDER_HOST=registry.opentofu.org

cd "$provider_dir"
if ! go test ./... -count=1 -run '^TestAcc' "$@"; then
  echo "== server log" >&2
  tail -50 "$work/server.log" >&2
  exit 1
fi
