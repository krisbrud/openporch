# openporch OpenTofu provider

The `openporch/openporch` provider manages an openporch organization's
platform-side objects as code. It is a thin client of the
[HTTP API](../docs/api.md), built on `terraform-plugin-framework` with plugin
protocol 6. Deployments are not resources: use `porch score deploy` or
`porch workload deploy` for those.

It is not published to a registry yet. This version has no resources; it
configures itself, checks its token, and offers the `openporch_current_user`
data source. Platform resources follow in tasks 7.2 and 7.3.

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

## Compatibility

Tested with OpenTofu 1.11.5 in CI.
