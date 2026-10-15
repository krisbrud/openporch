# openporch OpenTofu provider

The `openporch/openporch` provider manages an openporch organization's
platform-side objects as code. It is a thin client of the
[HTTP API](../docs/api.md), built on `terraform-plugin-framework` with plugin
protocol 6. Deployments are not resources: use `porch score deploy` or
`porch workload deploy` for those.

It is not published to a registry yet. It offers these resources, each
importable, and the `openporch_current_user` data source; more follow in tasks
7.2 and 7.3:

| Resource | Import ID |
| --- | --- |
| `openporch_application` | `APP` |
| `openporch_environment_type` | `TYPE` |
| `openporch_environment` | `APP/ENV` |
| `openporch_resource_class` | `TYPE/CLASS` |
| `openporch_value` | `APP/KEY` or `APP/ENV/KEY` |
| `openporch_cloud_account` | `ACCOUNT` |

A name left out of the configuration defaults to the ID when the object is
created; an imported object keeps the name it has. `destroy_acknowledgements`
lives only in the configuration, so the first apply after an import that sets
it records it as an in-place update that changes nothing on the server.

## Secrets

`openporch_value` and `openporch_cloud_account` take secrets in two ways. A
secret value (`secret_value`, or an entry of `secrets`) is written to the
built-in store, and the server keeps only a reference to it, which the provider
records in `written_secret_ref(s)`. A reference (`secret_ref`, or an entry of
`secret_refs`) points at a secret already in a store.

The server never returns a secret value, so the provider cannot compare one
with the configuration. It writes a secret again only when the configured value
changes, or when the version on the server is no longer the one it wrote, as
happens when someone writes the secret outside OpenTofu. An imported secret is
imported as its reference.

Secret values are kept in the OpenTofu state, as with any provider. Keep the
state in an encrypted backend that only platform administrators can read, or
use OpenTofu's state encryption, and prefer references where a secret already
lives in a store.

## Deleting applications and environments

Deleting an application or environment deletes the cloud resources it owns, so
the provider previews the deletion first and deletes nothing unless every
resource the preview marks is acknowledged in `destroy_acknowledgements`. The
error lists the entries to add, in the form `porch` prints them:

```hcl
resource "openporch_application" "shop" {
  id = "shop"
  destroy_acknowledgements = [
    "development:postgres.default#modules.web.externals.db=delete",
  ]
}
```

Add them, apply (which only records them), then remove the resource and apply
again. An environment takes the same entries without the `ENV:` prefix. The
`development` environment is created and deleted with its application and is
not a resource of its own. Deletion waits until the server has deleted every
resource; if one cannot be deleted, the error says so and the object stays.

## Configuration

```hcl
terraform {
  required_providers {
    openporch = {
      source = "openporch/openporch"
    }
  }
}

provider "openporch" {
  url    = "https://porch.example.com" # or OPENPORCH_URL
  org_id = "acme"                      # or OPENPORCH_ORG
  token  = var.openporch_token         # or OPENPORCH_TOKEN
}
```

Each argument may be left out and taken from its environment variable instead.
The token should be a service user's API token. Plain `http` URLs are only
accepted for loopback addresses. The provider checks the token when it is
configured, so a wrong setting fails at `tofu plan`.

## Developing

You need the Go version in `go.mod` and, for the smoke test, OpenTofu.

```sh
go test ./...                                      # unit tests
cargo build -p openporch-server --bin openporch    # from the repository root
scripts/smoke.sh ../target/debug/openporch         # load it into OpenTofu
scripts/acceptance.sh ../target/debug/openporch    # acceptance tests with OpenTofu
```

To use a local build, point a `dev_overrides` entry at the directory holding
the `terraform-provider-openporch` binary in the file `TF_CLI_CONFIG_FILE` names,
then run `tofu plan` without `tofu init`:

```hcl
provider_installation {
  dev_overrides {
    "registry.opentofu.org/openporch/openporch" = "/path/to/bin"
  }
  direct {}
}
```

`tofu init` refuses a provider that only a dev override provides, because it
still looks the provider up in the registry. To run `tofu init`, install the
build into a `filesystem_mirror` instead, as `scripts/smoke.sh` does.

## Licenses

`deny.toml` governs the Rust workspace only. The provider additionally depends,
unmodified, on HashiCorp's MPL-2.0 plugin libraries (`terraform-plugin-framework`,
`terraform-plugin-go`, `terraform-plugin-log`, `go-plugin`, `yamux` and their
registry helpers), which every OpenTofu provider in Go needs (design D11).
MPL-2.0 is accepted for `provider/` only; check new Go dependencies by hand
with `go list -m all` until a license check runs in CI.

## Compatibility

Tested with OpenTofu 1.11.5 in CI.
