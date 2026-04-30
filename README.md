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

## Concepts

- **ResourceType** — contract (output schema) for a class of provisioned thing.
- **Module** — OpenTofu code that implements a resource type. Multiple modules
  per type is the point: one for each environment variant.
- **ModuleRule** — selector mapping `(resource_type, env_matchers) → module`.
- **Provider** — central OpenTofu provider config, injected at the root TF.
- **Runner** — execution backend. v0 ships only `local-tofu`.
- **Manifest** — what the developer writes. Lists workloads + resources.
  Workloads are themselves resources, so any runtime (container/Lambda/VM/...)
  is just a module variant.

See `/Users/kristianbrudeli/aidev/humanitec-knowledge/` for the conceptual
model openporch borrows from.
