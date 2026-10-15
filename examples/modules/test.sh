#!/usr/bin/env bash
# Checks every module under examples/modules: `tofu fmt`, `tofu validate`, and
# `tofu test`, which plans and applies each module's tests/*.tftest.hcl against
# the AWS account or emulator the environment points at, then destroys what it
# created. CI points it at Floci:
#
#   AWS_ENDPOINT_URL=http://localhost:4566 AWS_ACCESS_KEY_ID=test \
#     AWS_SECRET_ACCESS_KEY=test AWS_REGION=us-east-1 examples/modules/test.sh
#
# Usage: examples/modules/test.sh [MODULE_DIR...]
# Needs `tofu` on PATH.
set -euo pipefail

root=$(cd "$(dirname "$0")" && pwd)
export TF_IN_AUTOMATION=1
export TF_PLUGIN_CACHE_DIR=${TF_PLUGIN_CACHE_DIR:-$root/.plugin-cache}
mkdir -p "$TF_PLUGIN_CACHE_DIR"

tofu fmt -check -recursive "$root"
if [ "$#" -eq 0 ]; then
  mapfile -t modules < <(find "$root" -name main.tf -not -path '*/.terraform/*' -printf '%h\n' | sort)
else
  modules=("$@")
fi
for module in "${modules[@]}"; do
  echo "== ${module#"$root"/}"
  (
    cd "$module"
    tofu init -backend=false -input=false -no-color >/dev/null
    tofu validate -no-color
    tofu test -no-color
  )
done
