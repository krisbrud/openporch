#!/usr/bin/env bash
# Checks every module under examples/modules: `tofu fmt`, `tofu validate`, and
# `tofu test`, which plans and applies each module's tests/*.tftest.hcl against
# the AWS account or emulator the environment points at, then destroys what it
# created. Each tests/errors/*.tftest.hcl must fail instead, with the message its
# `# expect-error:` line gives, which `expect_failures` cannot check. CI points
# it at Floci:
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
    for test in tests/errors/*.tftest.hcl; do
      [ -e "$test" ] || continue
      want=$(sed -n 's/^# expect-error: //p' "$test" | tr -s '[:space:]' ' ')
      [ -n "$want" ] || { echo "$test has no '# expect-error:' line" >&2; exit 1; }
      if out=$(tofu test -no-color -test-directory=tests/errors -filter="$test" 2>&1); then
        echo "$out" >&2
        echo "$test passed, but must fail with: $want" >&2
        exit 1
      fi
      if ! tr -s '[:space:]' ' ' <<<"$out" | grep -qF -- "$want"; then
        echo "$out" >&2
        echo "$test did not fail with: $want" >&2
        exit 1
      fi
      echo "$test... fails as expected"
    done
  )
done
