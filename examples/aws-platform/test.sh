#!/usr/bin/env bash
# Applies the example platform against a dev server and an AWS endpoint, checks
# what the server then holds, that a second plan changes nothing, and destroys
# it again. CI points AWS at Floci:
#
#   AWS_ENDPOINT_URL=http://localhost:4566 AWS_ACCESS_KEY_ID=test \
#     AWS_SECRET_ACCESS_KEY=test examples/aws-platform/test.sh target/debug/openporch
#
# Usage: examples/aws-platform/test.sh PATH_TO_OPENPORCH_BINARY
# Needs `go`, `tofu`, `curl` and `jq` on PATH.
set -euo pipefail

server_bin=$(realpath "${1:?usage: test.sh PATH_TO_OPENPORCH_BINARY}")
example=$(cd "$(dirname "$0")" && pwd)
provider_dir=$(cd "$example/../../provider" && pwd)
work=$(mktemp -d)
cleanup() {
  # Whatever an apply that failed half way created is destroyed too.
  if [ -n "${applying:-}" ]; then
    (cd "$work/platform" && tofu destroy -no-color -input=false -auto-approve "${vars[@]}" >/dev/null 2>&1) ||
      echo "could not destroy what the test created; see $work" >&2
  fi
  kill "${server_pid:-}" 2>/dev/null || true
  [ -n "${applying:-}" ] || rm -rf "$work"
}
trap cleanup EXIT

# The provider, built and installed from a filesystem mirror.
version=0.0.1
mirror="$work/mirror/registry.opentofu.org/openporch/openporch/$version/$(go env GOOS)_$(go env GOARCH)"
mkdir -p "$mirror" "$work/data"
(cd "$provider_dir" && go build -ldflags "-X main.version=$version" -o "$mirror/terraform-provider-openporch_v$version" .)

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
export OPENPORCH_TOKEN TF_IN_AUTOMATION=1
cat >"$work/tofurc" <<RC
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
export TF_CLI_CONFIG_FILE="$work/tofurc"
api() { curl -sf -H "Authorization: Bearer $OPENPORCH_TOKEN" "$OPENPORCH_URL/orgs/acme/$1"; }
fail() {
  echo "$1" >&2
  echo "== server log" >&2
  tail -50 "$work/server.log" >&2
  exit 1
}

mkdir "$work/platform"
cp "$example"/*.tf "$work/platform/"
if [ -n "${AWS_ENDPOINT_URL:-}" ]; then
  # An emulator serves buckets by path, not by host name.
  cat >"$work/platform/emulator_override.tf" <<'TF'
provider "aws" {
  s3_use_path_style = true
}
TF
fi
cd "$work/platform"
vars=(
  -var "trusted_principal_arn=${OPENPORCH_TEST_TRUSTED_PRINCIPAL:-arn:aws:iam::000000000000:root}"
  -var "external_id=a-test-external-id"
  -var "aws_region=${AWS_REGION:-us-east-1}"
)

echo "== tofu apply"
tofu init -no-color -input=false >/dev/null
applying=1
tofu apply -no-color -input=false -auto-approve "${vars[@]}" || fail "the platform did not apply"

echo "== the server has the platform"
defs=$(api resources/defs | jq -c '[.[] | {id, type, driver_type, driver_account}] | sort_by(.id)')
expected='[
  {"id":"aws-lambda-workload","type":"workload","driver_type":"openporch/opentofu","driver_account":"aws"},
  {"id":"base-env","type":"base-env","driver_type":"openporch/none","driver_account":null},
  {"id":"dns","type":"dns","driver_type":"openporch/echo","driver_account":null},
  {"id":"dynamodb-table","type":"dynamodb-table","driver_type":"openporch/opentofu","driver_account":"aws"},
  {"id":"dynamodb-table-policy","type":"aws-policy","driver_type":"openporch/opentofu","driver_account":"aws"},
  {"id":"s3","type":"s3","driver_type":"openporch/opentofu","driver_account":"aws"},
  {"id":"s3-policy","type":"aws-policy","driver_type":"openporch/opentofu","driver_account":"aws"},
  {"id":"sqs","type":"sqs","driver_type":"openporch/opentofu","driver_account":"aws"},
  {"id":"sqs-policy","type":"aws-policy","driver_type":"openporch/opentofu","driver_account":"aws"},
  {"id":"workload-none","type":"workload","driver_type":"openporch/none","driver_account":null}
]'
[ "$defs" = "$(jq -c . <<<"$expected")" ] || fail "unexpected definitions: $defs"
lambda=$(api resources/defs/aws-lambda-workload)
[ "$(jq -c '[.criteria[].class]' <<<"$lambda")" = '["aws-lambda"]' ] || fail "the Lambda definition does not match class aws-lambda: $lambda"
[ "$(jq -r '.driver_inputs.values.source.path' <<<"$lambda")" = examples/modules/aws-lambda-workload ] || fail "the Lambda definition's module: $lambda"
table=$(api resources/defs/dynamodb-table)
[ "$(jq -c '.provision | keys' <<<"$table")" = '["aws-policy.dynamodb-table"]' ] || fail "the table does not co-provision its policy: $table"
[ "$(jq -c '[.criteria[].class]' <<<"$(api resources/defs/sqs-policy)")" = '["sqs"]' ] || fail "the queue policy does not match class sqs"
api resources/types/workload/classes | jq -e 'any(.[]; .id == "aws-lambda")' >/dev/null || fail "no aws-lambda class"
api env-types | jq -e 'any(.[]; .id == "production")' >/dev/null || fail "no production environment type"
[ "$(jq -r '.type' <<<"$(api resources/accounts/aws)")" = aws-role ] || fail "the cloud account is not a role"
api resources/defs | jq -e 'all(.[]; .driver_type != "openporch/kubernetes")' >/dev/null || fail "a Kubernetes definition"

echo "== a second plan changes nothing"
set +e
tofu plan -no-color -input=false -detailed-exitcode "${vars[@]}" >"$work/plan.log"
status=$?
set -e
[ "$status" -eq 0 ] || { cat "$work/plan.log" >&2; fail "the second plan wants changes (exit $status)"; }

echo "== tofu destroy"
tofu destroy -no-color -input=false -auto-approve "${vars[@]}" >/dev/null || fail "the platform did not destroy"
applying=
[ "$(api resources/defs | jq length)" = 0 ] || fail "definitions are left after destroy"
echo "== ok"
