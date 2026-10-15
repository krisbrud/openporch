# openporch OpenTofu provider

The `openporch/openporch` provider manages an openporch organization's
platform-side objects as code. It is a thin client of the
[HTTP API](../docs/api.md), built on `terraform-plugin-framework` with plugin
protocol 6. Deployments are not resources: use `porch score deploy` or
`porch workload deploy` for those.

It is not published to a registry yet. [docs/](docs/index.md) holds the
reference for each resource, generated from the schema and the configurations
in `examples/`. It offers these resources, each importable, and the
`openporch_current_user` data source:

| Resource | Import ID |
| --- | --- |
| `openporch_application` | `APP` |
| `openporch_environment_type` | `TYPE` |
| `openporch_environment` | `APP/ENV` |
| `openporch_resource_class` | `TYPE/CLASS` |
| `openporch_value` | `APP/KEY` or `APP/ENV/KEY` |
| `openporch_cloud_account` | `ACCOUNT` |
| `openporch_user` (service users) | `USER` |
| `openporch_service_user_token` | `USER/TOKEN` |
| `openporch_application_user` | `APP/USER` |
| `openporch_environment_type_user` | `TYPE/USER` |
| `openporch_resource_definition` | `DEFINITION` |
| `openporch_resource_definition_criteria` | `DEFINITION/CRITERION` |

A name left out of the configuration defaults to the ID when the object is
created; an imported object keeps the name it has. `destroy_acknowledgements`
lives only in the configuration, so the first apply after an import that sets
it records it as an in-place update that changes nothing on the server.

## Resource definitions

A definition's plain inputs go in `driver_inputs`, an object nested as the
driver expects. Its criteria are separate resources, so a definition matches
nothing until one exists. openporch placeholders use the same `${…}` syntax as
OpenTofu, so write them with `$${…}` in HCL:

```hcl
resource "openporch_resource_definition" "redis" {
  id          = "redis"
  type        = "redis"
  driver_type = "openporch/opentofu"
  driver_inputs = {
    source    = { url = "https://github.com/acme/modules", path = "redis" }
    variables = { name = "$${context.app.id}-$${context.env.id}" }
  }
  driver_secrets = {
    variables = { auth_token = var.redis_auth_token }
  }
}

resource "openporch_resource_definition_criteria" "staging" {
  resource_definition_id = openporch_resource_definition.redis.id
  env_type               = "staging"

  # Changing the definition's type replaces it, and the server deletes its
  # criteria with it; this recreates them in the same apply.
  lifecycle {
    replace_triggered_by = [openporch_resource_definition.redis]
  }
}
```

## Secrets

`openporch_value`, `openporch_cloud_account` and
`openporch_resource_definition` take secrets in two ways. A
secret value (`secret_value`, or an entry of `secrets` or `driver_secrets`) is
written to the built-in store, and the server keeps only a reference to it,
which the provider records in `written_secret_ref(s)`. A reference
(`secret_ref`, or an entry of `secret_refs` or `driver_secret_refs`) points at a
secret already in a store. In `driver_secrets`, a whole placeholder such as
`$${resources.db.outputs.password}` is not a value: it stays a template that the
executor resolves at deployment.

The server never returns a secret value, so the provider cannot compare one
with the configuration. It writes a secret again only when the configured value
changes, or when the version on the server is no longer the one it wrote, as
happens when someone writes the secret outside OpenTofu. An imported secret is
imported as its reference.

## Protecting the state

Secret values and service user tokens are kept in the OpenTofu state, as with
any provider: OpenTofu marks them sensitive, which keeps them out of plans and
logs but not out of the state file. Whoever can read the state can use them.

- Keep the state in a remote backend with encryption at rest and access limited
  to platform administrators, or turn on OpenTofu's
  [state encryption](https://opentofu.org/docs/language/state/encryption/).
- Do not commit state files or plan files (`tofu plan -out`), which hold the
  same values.
- Prefer `secret_ref`/`secret_refs` where a secret already lives in a store, so
  only its reference reaches the state.
- Give tokens an `expires_at` and rotate them by changing it together with the
  token's `id`. The server returns a token only when it is created, so an
  imported token has no `token`, and a token revoked outside OpenTofu leaves the
  state and needs a new `id`, since a revoked token keeps its ID.

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
scripts/docs.sh                                    # regenerate docs/ (needs jq)
```

`docs/` is generated by `tfplugindocs` (run with `go tool`) from the schema's
descriptions, `templates/` and `examples/`; CI fails when it is out of date.
`scripts/docs.sh` also checks that the examples are formatted and valid as one
configuration.

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
The documentation generator `terraform-plugin-docs` and its dependency
`hashicorp/cli` (both MPL-2.0) are build tools
declared with `tool` in `go.mod` and are not part of the provider binary.
MPL-2.0 is accepted for `provider/` only; check new Go dependencies by hand
with `go list -m all` until a license check runs in CI.

## Compatibility

Tested with OpenTofu 1.11.5 in CI.
