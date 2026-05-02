# openporch

Open-source platform orchestrator. Register OpenTofu modules and rules; one
manifest deploys to different runtimes per environment (e.g. local Docker for
dev, AWS Lambda for prod) without changes.

Inspired by Humanitec's orchestration model; OpenTofu under the hood; CLI
first, GitOps daemon later.

## Status

v0 — CLI-only one-shot deploy pipeline. Not production ready.

## Quick start

```bash
go build -o bin/openporch ./cmd/openporch
./bin/openporch validate examples/apps/my-app/manifest.yaml \
    --platform examples/platform
./bin/openporch deploy examples/apps/my-app/manifest.yaml \
    --platform examples/platform \
    --project demo --env dev --env-type local
```

Requires `tofu` on PATH (`brew install opentofu`).

### Real Docker deploy (FastAPI demo)

`examples/apps/fastapi-demo/` is a runnable FastAPI app whose `/health`
endpoint queries a real Postgres container. The local-Docker modules under
`examples/platform/` provision both via `kreuzwerker/docker`.

```bash
docker build -t fastapi-demo:local examples/apps/fastapi-demo
./bin/openporch deploy examples/apps/fastapi-demo/manifest.yaml \
    --platform examples/platform --project demo --env dev --env-type local
curl http://localhost:8080/health   # {"db":"ok"}
./bin/openporch destroy examples/apps/fastapi-demo/manifest.yaml \
    --platform examples/platform --project demo --env dev --env-type local
```

### Integration tests

The end-to-end Docker test lives behind a build tag so the default
`go test ./...` stays hermetic:

```bash
go test -tags=integration -timeout=5m ./internal/deploy/...
```

It is skipped automatically when `docker version` fails.

## State layout

Everything openporch writes lives under `--state-root` (default `.openporch/`):

```
.openporch/
  state/<project>/<env>/<resource>/   # main.tf, module/main.tf, terraform.tfstate, outputs.json
  logs/<project>/<env>/<deployment>/  # per-resource tofu apply/destroy logs
  plugin-cache/                       # shared TF_PLUGIN_CACHE_DIR
```

Lifecycle:

- **Apply** is idempotent — re-running `deploy` rerenders `main.tf` and lets
  tofu reconcile.
- **Destroy** (`openporch destroy --prune`) reverses the deploy in reverse
  topo order, then removes the resource working directories. Logs are kept.
- The `plugin-cache/` is shared across deploys; safe to delete to reclaim
  disk, will be repopulated on next `init`.
- `outputs.json` may contain secrets (e.g. generated DB passwords); treat
  the state directory as secret-equivalent.

## Module layout

Modules are real OpenTofu modules on disk (or referenced remotely). The
example platform uses one directory per module under
`examples/platform/modules/`:

```
examples/platform/
  modules.yaml              # Module + ModuleRule registrations
  modules/<id>/main.tf      # the actual OpenTofu code
```

`module_source` in `modules.yaml` accepts local paths
(`./modules/postgres-local`), git URLs (`git::https://…`), and Terraform
registry refs — `tofu init` fetches remote sources at apply time.

## Concepts

- **ResourceType** — contract (output schema) for a class of provisioned thing.
- **Module** — OpenTofu code that implements a resource type, referenced via
  `module_source` (a local path under the platform directory, a git URL, or
  a Terraform registry ref). Multiple modules per type is the point: one for
  each environment variant.
- **ModuleRule** — selector mapping `(resource_type, env_matchers) → module`.
- **Provider** — central OpenTofu provider config, injected at the root TF.
- **Runner** — execution backend. v0 ships only `local-tofu`.
- **Manifest** — what the developer writes. Lists workloads + resources.
  Workloads are themselves resources, so any runtime (container/Lambda/VM/...)
  is just a module variant.

See `~/aidev/orchestrator-knowledge/` for the conceptual
model openporch borrows from.
