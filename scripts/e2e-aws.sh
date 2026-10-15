#!/usr/bin/env bash
# The opt-in end-to-end test against a real AWS account; see docs/e2e.md.
#
# It applies examples/aws-platform with a fresh name prefix, deploys
# examples/lambda-order-worker (a zip) and examples/lambda-api (an image) to the
# environment `e2e` of a new application `shop<hex>`, checks what exists in AWS, sends
# a signed order and waits for it to be stored, calls the API's URL, deploys a
# second artifact under the same key, replays the first deployment's snapshot
# and checks it runs the first artifact again, deletes the environment with a
# permit for each deletion, checks nothing it owned is left, prints what is kept
# and what it costs, and destroys the platform.
#
#   OPENPORCH_E2E_AWS_ACCOUNT=111122223333 AWS_REGION=eu-north-1 \
#     scripts/e2e-aws.sh target/debug/openporch target/debug/porch
#
# It creates real, billed resources, so it runs only when
# OPENPORCH_E2E_AWS_ACCOUNT names the account the current credentials are in.
# The definitions fetch the modules from GitHub at the commit being tested, so
# that commit must be pushed; OPENPORCH_E2E_MODULES_REV picks another.
#
# Needs `go`, `tofu`, `aws` (v2), `docker`, `git`, `zip`, `openssl`, `curl` and
# `jq` on PATH. If the test fails after it created anything, it deletes what it
# can; whatever it could not delete is listed, and the working directory, with
# the server's data and the resources' OpenTofu state, is kept.
set -euo pipefail

server_bin=$(realpath "${1:?usage: e2e-aws.sh PATH_TO_OPENPORCH PATH_TO_PORCH}")
porch_bin=$(realpath "${2:?usage: e2e-aws.sh PATH_TO_OPENPORCH PATH_TO_PORCH}")
repo=$(cd "$(dirname "$0")/.." && pwd)

# --- Guards -------------------------------------------------------------------

if env | grep -q '^AWS_ENDPOINT_URL'; then
  echo "an AWS_ENDPOINT_URL variable is set; this test runs against real AWS only (examples/aws-platform/test.sh runs against an emulator)" >&2
  exit 2
fi
: "${OPENPORCH_E2E_AWS_ACCOUNT:?set OPENPORCH_E2E_AWS_ACCOUNT to the AWS account ID this test may create billed resources in}"
region=${AWS_REGION:-${AWS_DEFAULT_REGION:-}}
: "${region:?set AWS_REGION}"
export AWS_REGION=$region AWS_PAGER=""
caller=$(aws sts get-caller-identity --output json)
account=$(jq -r .Account <<<"$caller")
if [ "$account" != "$OPENPORCH_E2E_AWS_ACCOUNT" ]; then
  echo "the credentials are for account $account, not OPENPORCH_E2E_AWS_ACCOUNT=$OPENPORCH_E2E_AWS_ACCOUNT" >&2
  exit 2
fi
# The server runs with these credentials, so the platform's role trusts their
# identity. A role session (SSO, assume-role) is trusted through its role, whose
# ARN, with its path, comes from IAM; OPENPORCH_E2E_TRUSTED_PRINCIPAL overrides.
principal=${OPENPORCH_E2E_TRUSTED_PRINCIPAL:-}
if [ -z "$principal" ]; then
  principal=$(jq -r .Arn <<<"$caller")
  if [[ $principal =~ ^arn:[^:]+:sts::[0-9]+:assumed-role/([^/]+)/ ]]; then
    principal=$(aws iam get-role --role-name "${BASH_REMATCH[1]}" --query Role.Arn --output text) || {
      echo "could not read the role of $(jq -r .Arn <<<"$caller"); set OPENPORCH_E2E_TRUSTED_PRINCIPAL to its ARN" >&2
      exit 2
    }
  fi
fi

# The definitions fetch the modules from the platform's `modules_source`, so the
# revision must be there.
modules_source=https://github.com/krisbrud/openporch
rev=${OPENPORCH_E2E_MODULES_REV:-$(git -C "$repo" rev-parse HEAD)}
if ! git -C "$repo" fetch -q --depth=1 "$modules_source" "$rev" 2>/dev/null; then
  echo "$modules_source has no $rev, and the definitions fetch the modules from there; push it or set OPENPORCH_E2E_MODULES_REV" >&2
  exit 2
fi
if [ -n "$(git -C "$repo" status --porcelain -- examples)" ]; then
  echo "warning: examples/ has uncommitted changes; the platform comes from the working tree but the modules from $rev" >&2
fi

# --- Working directory, cleanup and logging ----------------------------------

work=$(mktemp -d)
run=$(openssl rand -hex 3)
prefix="ope2e-$run"
# AWS names start with `<app>-<env>-`, so each run has its own application.
app="shop$run"
env=e2e
repository="$prefix-lambda-api"
results="$work/results.md"
started=$(date -u +%Y-%m-%dT%H:%M:%SZ)
echo "== working in $work, name prefix $prefix, account $account, region $region"

step() { echo "== $*"; printf -- '- %s\n' "$*" >>"$results"; }
fail() {
  echo "FAILED: $1" >&2
  if [ -f "$work/server.log" ]; then
    echo "== server log" >&2
    tail -80 "$work/server.log" >&2
  fi
  exit 1
}

porch() { "$porch_bin" "$@"; }
active() { porch get active-resources "$1" -o json; }

# Signs a body with the signing key, which is passed in the environment rather
# than on a command line.
sign() {
  KEY="$key" python3 -c 'import hashlib, hmac, os, sys
print(hmac.new(os.environ["KEY"].encode(), sys.stdin.buffer.read(), hashlib.sha256).hexdigest())'
}
send_order() {
  local body="{\"id\": \"$1\", \"total\": 30.5}" signature
  signature=$(printf %s "$body" | sign)
  aws sqs send-message --queue-url "$queue_url" --message-body "$body" \
    --message-attributes "signature={DataType=String,StringValue=$signature}" >/dev/null
}
await_order() {
  for _ in $(seq 1 60); do
    [ "$(aws dynamodb get-item --table-name "$table" --key "{\"id\": {\"S\": \"$1\"}}" --query Item.total.N --output text)" = 30.5 ] && return 0
    sleep 5
  done
  return 1
}

# Deletes the environment with a permit for every deletion its preview marks.
delete_env() {
  local preview plan code status args=()
  # A deployment that is still running, after a timeout or an interrupt, has to
  # finish first: deleting during one is refused.
  for _ in $(seq 1 180); do
    status=$(porch get deploys +0 -o json 2>/dev/null | jq -r .status) || status=
    case "$status" in pending | in_progress) sleep 10 ;; *) break ;; esac
  done
  preview=$(porch delete env "$env" --preview) || return 1
  echo "$preview" >"$work/delete-preview.txt"
  # With changes that need permits, the preview ends with the command that
  # carries it out: `--plan ID` and one `--ack '...'` per deletion.
  plan=$(grep -o -- '--plan [^ ]*' <<<"$preview" | head -1 | cut -d' ' -f2)
  [ -z "$plan" ] || args+=(--plan "$plan")
  while read -r ack; do
    [ -z "$ack" ] || args+=(--ack "$ack")
  done < <(grep -o -- "--ack '[^']*'" <<<"$preview" | sed -E "s/^--ack '(.*)'$/\\1/")
  porch delete env "$env" "${args[@]}" --comment "e2e cleanup" >/dev/null || return 1
  # The deletion runs like a deployment; the environment is gone (404) once it
  # has deleted every resource, and stays `deletion_failed` if one fails.
  for _ in $(seq 1 180); do
    code=$(curl -s -o "$work/env.json" -w '%{http_code}' -H @"$work/auth" "$OPENPORCH_URL/orgs/acme/apps/$app/envs/$env")
    [ "$code" != 404 ] || { env_deployed=; return 0; }
    [ "$(jq -r .state "$work/env.json" 2>/dev/null)" != deletion_failed ] || { cat "$work/env.json"; return 1; }
    sleep 5
  done
  return 1
}

tofu_platform() { (cd "$work/platform" && TF_CLI_CONFIG_FILE="$work/tofurc" tofu "$@" -no-color -input=false -var "external_id=$external_id"); }
destroy_platform() {
  # Every version of every artifact goes first, or the bucket cannot be deleted.
  # An apply that failed early may not have created the bucket.
  if aws s3api head-bucket --bucket "$bucket" >/dev/null 2>&1; then
    empty_bucket "$bucket" || return 1
  fi
  tofu_platform destroy -auto-approve >"$work/destroy.log" 2>&1
}
empty_bucket() {
  local objects
  while :; do
    objects=$(aws s3api list-object-versions --bucket "$1" --max-items 500 --output json |
      jq -c '[(.Versions // [])[], (.DeleteMarkers // [])[] | {Key, VersionId}]') || return 1
    [ "$(jq length <<<"$objects")" -gt 0 ] || return 0
    aws s3api delete-objects --bucket "$1" --delete "{\"Objects\": $objects, \"Quiet\": true}" >/dev/null || return 1
  done
}

# What is still to delete if the test stops early.
env_deployed=
platform_applied=
repository_created=
cleanup() {
  local status=$? left=
  set +e
  if [ -n "$env_deployed" ]; then
    echo "== cleanup: deleting the environment" >&2
    delete_env >&2 || left+=" the environment $app/$env (its OpenTofu state is under $work/data);"
  fi
  if [ -n "$app_created" ] && [ -z "$left" ]; then
    porch delete app "$app" >/dev/null 2>&1 || echo "could not delete the application $app; the platform's destroy may fail on its definitions" >&2
  fi
  if [ -n "$repository_created" ]; then
    aws ecr delete-repository --repository-name "$repository" --force >/dev/null ||
      left+=" the ECR repository $repository;"
  fi
  if [ -n "$platform_applied" ]; then
    if [ -z "$left" ]; then
      echo "== cleanup: destroying the platform" >&2
      destroy_platform >&2 || left+=" the platform (state in $work/platform);"
    else
      left+=" the platform (state in $work/platform), kept because resources that use it are left;"
    fi
  fi
  kill "${server_pid:-}" 2>/dev/null
  wait "${server_pid:-}" 2>/dev/null
  if [ -n "$left" ]; then
    echo "NOT DELETED:$left see $work and docs/e2e.md" >&2
    [ "$status" -ne 0 ] || status=1
  elif [ "$status" -eq 0 ]; then
    rm -rf "$work"
  else
    echo "everything the test created was deleted; the logs are in $work" >&2
  fi
  exit "$status"
}
app_created=
bucket=
trap cleanup EXIT

# --- Server, CLI and provider -------------------------------------------------

version=0.0.1
mirror="$work/mirror/registry.opentofu.org/openporch/openporch/$version/$(go env GOOS)_$(go env GOARCH)"
mkdir -p "$mirror" "$work/data" "$work/bin"
(cd "$repo/provider" && go build -ldflags "-X main.version=$version" -o "$mirror/terraform-provider-openporch_v$version" .)
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

# The server takes its AWS identity from this environment, as a host would from
# its instance role, and assumes the platform's role with it.
port=$((20000 + RANDOM % 12000))
OPENPORCH_ORG=acme OPENPORCH_DATA_DIR="$work/data" OPENPORCH_LISTEN="127.0.0.1:$port" \
  "$server_bin" serve >"$work/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 100); do
  curl -sf "http://127.0.0.1:$port/healthz" >/dev/null && break
  kill -0 "$server_pid" 2>/dev/null || break
  sleep 0.2
done
curl -sf "http://127.0.0.1:$port/healthz" >/dev/null || fail "the server did not start"

export OPENPORCH_URL="http://127.0.0.1:$port" OPENPORCH_ORG=acme OPENPORCH_APP=$app OPENPORCH_ENV=$env
OPENPORCH_TOKEN=$(cat "$work/data/bootstrap-token")
export OPENPORCH_TOKEN TF_IN_AUTOMATION=1
# The token goes to curl in a file, not on its command line.
(umask 077 && printf 'Authorization: Bearer %s\n' "$OPENPORCH_TOKEN" >"$work/auth")

# --- Platform ------------------------------------------------------------------

mkdir "$work/platform"
cp "$repo"/examples/aws-platform/*.tf "$work/platform/"
# Every variable but the external ID is in a file, so a destroy by hand needs
# only `-var external_id=<any 16 characters>`.
jq -n --arg region "$region" --arg principal "$principal" --arg prefix "$prefix" \
  --arg source "$modules_source" --arg rev "$rev" \
  '{aws_region: $region, trusted_principal_arn: $principal, name_prefix: $prefix, modules_source: $source, modules_rev: $rev}' \
  >"$work/platform/e2e.auto.tfvars.json"
external_id=$(openssl rand -hex 16)

{
  echo "# openporch e2e against AWS"
  echo
  echo "Started $started in account $account, region $region, modules at $rev, prefix $prefix."
  echo
} >"$results"

step "Apply examples/aws-platform"
(cd "$work/platform" && TF_CLI_CONFIG_FILE="$work/tofurc" tofu init -no-color -input=false >/dev/null)
bucket="$prefix-artifacts-$account"
platform_applied=1
tofu_platform apply -auto-approve >"$work/apply.log" 2>&1 || { tail -40 "$work/apply.log" >&2; fail "the platform did not apply"; }
[ "$(cd "$work/platform" && tofu output -raw artifact_bucket)" = "$bucket" ] || fail "unexpected artifact bucket"
# A new role's trust policy takes a few seconds to reach STS.
for attempt in $(seq 1 12); do
  check=$(porch api post /orgs/acme/resources/accounts/aws/check 2>&1) && break
  [ "$attempt" -lt 12 ] || fail "openporch cannot assume the platform's role: $check"
  sleep 5
done

# --- Application, environment and secret ---------------------------------------

step "Create the application, the environment and the signing key"
porch create app "$app" >/dev/null
app_created=1
porch create env "$env" --type development >/dev/null
key=$(openssl rand -hex 32)
printf '{"key": "ORDER_SIGNING_KEY", "secret_ref": {"value": "%s"}}' "$key" |
  porch api post "/orgs/acme/apps/$app/values" -d @- >/dev/null || fail "could not create the signing key"

# --- Artifacts ------------------------------------------------------------------

worker_zip() {
  # $1 is the build number; it goes into a comment so the two zips differ.
  local out="$work/order-worker-$1.zip"
  mkdir -p "$work/build-$1"
  cp "$repo"/examples/lambda-order-worker/app/{app.py,secret_refs.py} "$work/build-$1/"
  printf '\n# e2e build %s\n' "$1" >>"$work/build-$1/app.py"
  (cd "$work/build-$1" && zip -X -q "$out" app.py secret_refs.py)
  echo "$out"
}
code_sha() { openssl dgst -sha256 -binary "$1" | base64; }
upload() {
  # Both builds go to the same key: openporch pins the version it saw.
  aws s3api put-object --bucket "$bucket" --key order-worker/e2e.zip --body "$1" --output json | jq -r .VersionId
}

step "Upload the first worker artifact and push the API image"
zip1=$(worker_zip 1)
version1=$(upload "$zip1")
registry="$account.dkr.ecr.$region.amazonaws.com"
aws ecr create-repository --repository-name "$repository" >/dev/null
repository_created=1
aws ecr get-login-password | docker login --username AWS --password-stdin "$registry" >/dev/null
docker build -q --platform linux/amd64 --provenance=false -t "$registry/$repository:1" "$repo/examples/lambda-api/app" >/dev/null
docker push -q "$registry/$repository:1" >/dev/null

# --- First deployments ----------------------------------------------------------

step "Deploy the order worker and the API with porch workload deploy --wait"
env_deployed=1
porch workload deploy -f "$repo/examples/lambda-order-worker/workload.yaml" \
  --artifact "main=s3://$bucket/order-worker/e2e.zip" --comment "e2e build 1" --wait --timeout 30m ||
  fail "the order worker did not deploy"
porch workload deploy -f "$repo/examples/lambda-api/workload.yaml" \
  --image "main=$registry/$repository:1" --comment "e2e api" --wait --timeout 30m ||
  fail "the API did not deploy"
# The deployment that is replayed later: both workloads, with the first zip.
# Replaying one without the API would delete it, which needs a permit.
first_deploy=$(porch get deploys +0 -o json | jq -r .id)

worker=$(active workload/modules.order-worker) || fail "no active order worker"
api=$(active workload/modules.lambda-api) || fail "no active API"
table=$(active dynamodb-table/modules.order-worker.externals.orders | jq -r .outputs.name)
queue_url=$(active sqs/modules.order-worker.externals.inbox | jq -r .outputs.url)
queue_arn=$(active sqs/modules.order-worker.externals.inbox | jq -r .outputs.arn)
worker_fn=$(jq -r .outputs.function_name <<<"$worker")
api_fn=$(jq -r .outputs.function_name <<<"$api")
url=$(jq -r .outputs.url <<<"$api")
echo "$worker_fn $api_fn $table $queue_url $url" >"$work/names.txt"

step "Check that the functions, table, queue, schedule and event source mapping exist"
[ "$(aws lambda get-function --function-name "$worker_fn" --query Configuration.State --output text)" = Active ] || fail "the worker is not active"
[ "$(aws lambda get-function --function-name "$api_fn" --query Configuration.PackageType --output text)" = Image ] || fail "the API is not an image function"
[ "$(aws lambda get-function --function-name "$worker_fn" --query Configuration.CodeSha256 --output text)" = "$(code_sha "$zip1")" ] ||
  fail "the worker does not run the first artifact"
aws lambda get-function-configuration --function-name "$worker_fn" --query 'Environment.Variables' --output json >"$work/worker-env.json"
jq -e --arg t "$table" '.TABLE == $t and (.SIGNING_KEY_SECRET_REF | startswith("/openporch/")) and (has("SIGNING_KEY") | not)' "$work/worker-env.json" >/dev/null ||
  fail "the worker's environment is not as expected: $(jq -c 'del(.SIGNING_KEY)' "$work/worker-env.json")"
porch logs deploy +0 >"$work/deploy.log" 2>&1
porch get active-resources -o json >"$work/active.json"
grep -qF -f <(printf '%s\n' "$key") "$work/worker-env.json" "$work/server.log" "$work/deploy.log" "$work/active.json" &&
  fail "the signing key reached the function's environment, the server log, the deployment log or the active resources"
[ "$(aws dynamodb describe-table --table-name "$table" --query Table.TableStatus --output text)" = ACTIVE ] || fail "the table is not active"
aws sqs get-queue-attributes --queue-url "$queue_url" --attribute-names QueueArn >/dev/null || fail "the queue does not exist"
mapping=$(aws lambda list-event-source-mappings --function-name "$worker_fn" --event-source-arn "$queue_arn" --query 'EventSourceMappings[0]' --output json)
[ "$(jq -r .State <<<"$mapping")" = Enabled ] || fail "the queue's event source mapping is not enabled: $mapping"
schedules=$(aws scheduler list-schedules --name-prefix "$(cut -c1-40 <<<"$worker_fn")" --query 'Schedules[].Name' --output json)
[ "$(jq length <<<"$schedules")" = 1 ] || fail "expected one schedule for the worker: $schedules"
schedule=$(aws scheduler get-schedule --name "$(jq -r '.[0]' <<<"$schedules")" --output json)
[ "$(jq -r '.ScheduleExpression + " " + .ScheduleExpressionTimezone' <<<"$schedule")" = "cron(0 3 * * ? *) Europe/Oslo" ] ||
  fail "the schedule is not as expected: $(jq -c '{ScheduleExpression, ScheduleExpressionTimezone}' <<<"$schedule")"

step "Send a signed order and see the worker store it"
send_order e2e-1
await_order e2e-1 || fail "the order was not stored within 5 minutes"

step "Call the API's URL"
# A new function URL's permission can take a few seconds to apply.
get() { curl -s --retry 6 --retry-all-errors --retry-delay 5 -f "$1"; }
[ "$(get "$url" | jq -r .message)" = "Hello from openporch" ] || fail "the API did not greet at $url"
[ "$(get "${url%/}/health" | jq -r .status)" = ok ] || fail "the API's /health did not answer"
[ "$(curl -s -o /dev/null -w '%{http_code}' "${url%/}/nope")" = 404 ] || fail "the API did not answer 404 for an unknown path"

# --- Second artifact and replay --------------------------------------------------

step "Deploy a second artifact under the same key"
zip2=$(worker_zip 2)
version2=$(upload "$zip2")
[ "$version1" != "$version2" ] || fail "the bucket did not keep a new version"
porch workload deploy -f "$repo/examples/lambda-order-worker/workload.yaml" \
  --artifact "main=s3://$bucket/order-worker/e2e.zip" --comment "e2e build 2" --wait --timeout 30m ||
  fail "the second artifact did not deploy"
[ "$(aws lambda get-function --function-name "$worker_fn" --query Configuration.CodeSha256 --output text)" = "$(code_sha "$zip2")" ] ||
  fail "the worker does not run the second artifact"
send_order e2e-2
await_order e2e-2 || fail "the second build did not store an order"

step "Replay the first deployment's snapshot and check it runs the first artifact"
porch deploy deploy "$first_deploy" --replay snapshot --comment "e2e replay" --wait --timeout 30m ||
  fail "the replay did not deploy"
[ "$(aws lambda get-function --function-name "$worker_fn" --query Configuration.CodeSha256 --output text)" = "$(code_sha "$zip1")" ] ||
  fail "the replay does not run the first artifact (version $version1)"
send_order e2e-3
await_order e2e-3 || fail "the replayed build did not store an order"

# --- Deletion ------------------------------------------------------------------

step "Delete the environment with a permit for each deletion"
delete_env || fail "the environment was not deleted; see $work/delete-preview.txt"
echo "  ($(grep -o -- "--ack '" "$work/delete-preview.txt" | wc -l) permits)" >>"$results"

step "Check that nothing the environment owned is left"
left=()
aws lambda get-function --function-name "$worker_fn" >/dev/null 2>&1 && left+=("function $worker_fn")
aws lambda get-function --function-name "$api_fn" >/dev/null 2>&1 && left+=("function $api_fn")
aws dynamodb describe-table --table-name "$table" >/dev/null 2>&1 && left+=("table $table")
# SQS can answer for a deleted queue for up to a minute.
for _ in $(seq 1 12); do
  aws sqs get-queue-attributes --queue-url "$queue_url" >/dev/null 2>&1 || break
  sleep 10
done
aws sqs get-queue-attributes --queue-url "$queue_url" >/dev/null 2>&1 && left+=("queue $queue_url")
for name in $(jq -r '.[]' <<<"$schedules"); do
  aws scheduler get-schedule --name "$name" >/dev/null 2>&1 && left+=("schedule $name")
done
for fn in "$worker_fn" "$api_fn"; do
  [ "$(aws lambda list-event-source-mappings --function-name "$fn" --query 'length(EventSourceMappings)' --output text 2>/dev/null || echo 0)" = 0 ] ||
    left+=("event source mappings of $fn")
  [ "$(aws logs describe-log-groups --log-group-name-prefix "/aws/lambda/$fn" --query 'length(logGroups)' --output text)" = 0 ] ||
    left+=("log group /aws/lambda/$fn")
  [ "$(aws ssm get-parameters-by-path --path "/openporch/$fn" --recursive --query 'length(Parameters)' --output text)" = 0 ] ||
    left+=("parameters under /openporch/$fn")
done
roles=$(aws iam list-roles --path-prefix /openporch/ --query "Roles[?starts_with(RoleName, '$app-$env-')].RoleName" --output json)
[ "$(jq length <<<"$roles")" = 0 ] || left+=("roles $(jq -c . <<<"$roles")")
policies=$(aws iam list-policies --scope Local --path-prefix /openporch/ --query "Policies[?starts_with(PolicyName, '$prefix-')].PolicyName" --output json)
[ "$(jq length <<<"$policies")" = 0 ] || left+=("policies $(jq -c . <<<"$policies")")
tagged=$(aws resourcegroupstaggingapi get-resources --tag-filters "Key=openporch:app,Values=$app" "Key=openporch:env,Values=$env" \
  --query 'ResourceTagMappingList[].ResourceARN' --output json)
# The tagging API lists deleted resources for a while; anything it names is checked above.
echo "  tagging API still lists: $(jq -c . <<<"$tagged")" >>"$results"
[ "${#left[@]}" = 0 ] || fail "left after the environment was deleted: ${left[*]}"

step "List what is kept, and what it costs"
{
  echo
  echo "## Kept after the environment was deleted"
  echo
  versions=$(aws s3api list-object-versions --bucket "$bucket" --output json |
    jq '{count: (.Versions // [] | length), bytes: ([(.Versions // [])[].Size] | add // 0)}')
  echo "- Artifact bucket \`$bucket\`: $(jq .count <<<"$versions") object versions, $(jq .bytes <<<"$versions") bytes (S3 storage, billed until the platform is destroyed or a lifecycle rule expires them)"
  images=$(aws ecr describe-images --repository-name "$repository" --query 'imageDetails[].imageSizeInBytes' --output json)
  echo "- ECR repository \`$repository\`: $(jq length <<<"$images") images, $(jq 'add // 0' <<<"$images") bytes (ECR storage, billed; this test deletes the repository)"
  echo "- OpenTofu state of the deleted resources: $(find "$work/data" -name terraform.tfstate | wc -l) files under the server's data directory (local disk, not billed)"
  echo "- The platform: role, boundary policy and bucket (no charge while idle except the bucket's storage)"
} >>"$results"

porch delete app "$app" >/dev/null || fail "the application was not deleted"
app_created=

step "Destroy the platform"
aws ecr delete-repository --repository-name "$repository" --force >/dev/null && repository_created=
destroy_platform || fail "the platform did not destroy; see $work/destroy.log"
platform_applied=

echo >>"$results"
echo "Finished $(date -u +%Y-%m-%dT%H:%M:%SZ): passed." >>"$results"
cat "$results"
out=${OPENPORCH_E2E_RESULTS:-$repo/target/e2e-aws-results.md}
mkdir -p "$(dirname "$out")"
cp "$results" "$out"
echo "== ok"
