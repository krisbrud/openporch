#!/usr/bin/env bash
# Builds the release binaries for the host with the locked inputs and packages
# them into OUT_DIR:
#
#   openporch-VERSION-TARGET.tar.gz                       openporch, porch, LICENSE, README.md
#   terraform-provider-openporch_VERSION_GOOS_GOARCH.zip  the OpenTofu provider, in the
#                                                         layout a filesystem mirror reads
#
# TARGET names the Rust target the host builds; GOOS and GOARCH the provider's
# platform (defaults: the host's). The release workflow runs it on each
# platform; docs/releasing.md describes it.
set -euo pipefail

out=${1:?usage: release-archive.sh OUT_DIR}
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

version=$(cargo metadata --locked --no-deps --format-version 1 \
  | jq -r '.packages[] | select(.name == "openporch-server") | .version')
target=${TARGET:-$(rustc -vV | sed -n 's/^host: //p')}
goos=${GOOS:-$(go env GOOS)}
goarch=${GOARCH:-$(go env GOARCH)}

# Paths of the build machine stay out of the binaries.
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$root=. --remap-path-prefix=$HOME=~"
cargo build --release --locked --target "$target" -p openporch-server -p openporch-cli

mkdir -p "$out"
out=$(cd "$out" && pwd)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

name="openporch-$version-$target"
mkdir "$stage/$name"
cp "target/$target/release/openporch" "target/$target/release/porch" LICENSE README.md "$stage/$name/"
tar -C "$stage" -cf - "$name" | gzip -n > "$out/$name.tar.gz"

provider="terraform-provider-openporch_v$version"
(cd provider && CGO_ENABLED=0 GOOS="$goos" GOARCH="$goarch" GOFLAGS=-mod=readonly \
  go build -trimpath -ldflags "-s -w -X main.version=$version" -o "$stage/$provider" .)
(cd "$stage" && zip -q -X "$out/terraform-provider-openporch_${version}_${goos}_${goarch}.zip" "$provider")

ls -l "$out"
