# Tasks

## 1. Repository scaffolding
- [ ] 1.1 Create the Cargo workspace with crates `core`, `store`, `drivers`, `k8s`, `engine`, `server`, `client`, `cli`, an Apache-2.0 LICENSE and a README describing openporch; verify `cargo build` and `cargo test` pass on the empty workspace
- [ ] 1.2 Add CI (GitHub Actions) running `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, and a Postgres service for store tests; verify the workflow is green on the branch
- [ ] 1.3 Add `rust-toolchain.toml`, `rustfmt.toml`, `deny.toml` (license allow-list) and a `justfile` with `dev`, `test`, `e2e` recipes; verify `cargo deny check` passes

## 2. Core domain and pure logic
- [ ] 2.1 Implement domain types and ID validation (org, app, env, env type, resource descriptor, GUResID, definition, criteria, cloud account) with serde; verify unit tests cover the ID regex and descriptor parsing from `api-access` and `active-resources`
- [ ] 2.2 Implement the placeholder parser and resolver (`context`, `resources` dot and bracket forms, `externals`, `shared`, `values`, `pod`, `$\{` escape, typed substitution, secret tainting); verify property and example tests cover every scenario in `placeholders`
- [ ] 2.3 Implement criteria matching with specificity weights, no-match and ambiguity errors; verify tests reproduce the `static-dev-dns` vs `random-dns` example and tie detection
- [ ] 2.4 Implement deployment set canonicalization (JCS) and SHA-256 IDs, delta application order, and set diff producing a delta; verify round-trip tests `apply(diff(a,b), a) == b` and fixture IDs
- [ ] 2.5 Implement Score v1b1 parsing for the supported subset, rejection of unsupported fields, dependency mapping (private/shared IDs) and placeholder translation with output-schema validation; verify tests cover every scenario in `score-workloads`
- [ ] 2.6 Implement resource graph construction (workload, dependencies, implicit nodes, resource references with inherited class/ID, co-provisioning flags), cycle detection and topological ordering; verify tests cover every scenario in `resource-graph`
- [ ] 2.7 Implement the built-in resource type catalog with input/output schemas and output validation; verify a test asserts the type list and the `postgres` schema from `resource-catalog`

## 3. Storage and control plane API
- [ ] 3.1 Implement the `Store` trait and sqlx migrations for apps, env types, envs, types, classes, definitions, criteria, accounts, drivers, values, sets, deltas, deployments, deployment resources, active resources and logs; verify store tests pass on SQLite and Postgres in CI
- [ ] 3.2 Implement secret encryption at rest (AES-256-GCM, master key from env or file, key ID stored) used by the store for all secret columns; verify tests show ciphertext at rest and decryption on read
- [ ] 3.3 Implement the axum server with config loading, bearer-token auth, the single-org guard, the error format, `/healthz` and `/version`; verify HTTP tests cover every scenario in `api-access`
- [ ] 3.4 Implement application, environment type and environment endpoints including default `development`, clone-from-deployment drafts and delete guards; verify HTTP tests cover `applications-and-environments` (deprovision-on-delete is verified in 5.5)
- [ ] 3.5 Implement resource type, class, definition, criteria, driver registry and cloud account endpoints with in-use guards and secret redaction on read; verify HTTP tests cover `resource-catalog`, `resource-definitions` (structure, secrets, criteria management, delete-in-use) and `cloud-accounts` (create, delete-in-use)
- [ ] 3.6 Implement AWS cloud account credential resolution (`aws` static, `aws-role` AssumeRole with external ID) and the `check` endpoint with `GetCallerIdentity`; verify unit tests with a mocked STS client and a documented manual check against a real account
- [ ] 3.7 Implement set, delta and diff endpoints and shared values endpoints; verify HTTP tests cover `deployment-sets-and-deltas` except Score deploy (verified in 6.2)
- [ ] 3.8 Write `docs/api.md` from the implemented routes with request and response examples; verify every documented example runs against a dev server in a doc test script

## 4. Drivers
- [ ] 4.1 Define the `Driver` trait, request/response types, cookie handling, timeouts and the driver registry; verify unit tests cover async polling and cookie round trip from `drivers`
- [ ] 4.2 Implement `openporch/echo`; verify tests show values and secrets echoed as outputs
- [ ] 4.3 Implement `openporch/template` on minijinja with `init`, `outputs`, `secrets`, `manifests`, `cookie` templates and document the Go-template to Jinja translation table in `docs/drivers/template.md`; verify tests cover deterministic passwords via cookie and manifest emission
- [ ] 4.4 Implement the external HTTP driver adapter with the 0.9.3-compatible JSON shapes, status codes and cookie headers; verify tests against an in-test mock driver cover 200/202/204/400/404/409
- [ ] 4.5 Implement `openporch/opentofu`: working dir per GUResID, git clone at rev, `script` as override, `files`, variables files (secret file unlogged), sanitized env, `credentials_config` and default AWS env, `tofu init/apply/output/destroy`, failure reporting; verify tests run a local-only module (random provider) end to end and assert redaction of a sensitive output in logs
- [ ] 4.6 Implement OpenTofu state handling: backend override for `s3` (bucket, region, lock table from config) and `local` for development, ignoring a `local` backend in code; verify a test applies a module twice against a local backend with no changes on the second run, and an opt-in test does the same against a real S3 bucket
- [ ] 4.7 Write `docs/drivers/opentofu.md` with input reference and an RDS postgres example; verify the example definition validates against the server

## 5. Deployment engine and active resources
- [ ] 5.1 Implement the deployment job: trigger endpoint (delta, set, deploy ID, `+n`), concurrency guard per environment, graph build and persistence, `full` mode, rejection of `incremental`; verify HTTP tests cover triggering, concurrent `409` and mode rejection from `deployments`
- [ ] 5.2 Implement the graph walker with bounded parallelism, three attempts, pending dependents on failure, deployment timeout and interrupted-on-restart marking; verify engine tests with fake drivers cover the one-branch-fails scenario and parallel branches
- [ ] 5.3 Implement active resource bookkeeping: GUResID allocation and reuse, outputs and cookies stored, usage tracking, unreferenced persistence, scheduled deletion, deprovision ordering, detach; verify engine tests cover every scenario in `active-resources`
- [ ] 5.4 Implement deployment logs with the redaction layer and the logs and graph inspection endpoints; verify tests show `[REDACTED]` for secret values in captured driver output
- [ ] 5.5 Implement environment and application deletion through the engine (reverse-order deprovisioning, failure leaves the environment with the error); verify engine tests cover both scenarios in `applications-and-environments`
- [ ] 5.6 Implement the `openporch/none` workload profile (resolve the workload's placeholders, store the rendered container config as the workload's outputs, no runtime target); verify engine tests show resolved variables and secret tainting from `score-workloads`
- [ ] 5.7 Run an in-memory end-to-end test: definitions using echo and template, a Score workload with a postgres dependency, deploy, roll back with `+1`; verify the set IDs and active resources match `deployments` and `deployment-sets-and-deltas` scenarios using the `openporch/none` profile

## 6. CLI
- [ ] 6.1 Implement the typed API client crate and `porch` configuration (flags, env vars, config file, `login`); verify tests cover precedence and the env-var scenario in `cli`
- [ ] 6.2 Implement `get|create|delete` object commands with table, YAML and JSON output and `kind: Definition` file input; verify tests against a dev server cover creating a definition from YAML
- [ ] 6.3 Implement `score deploy` (extensions, `--image`, `--property`, `--comment`, `--wait` with status streaming and non-zero exit on failure), `deploy delta`, `deploy deploy`, `diff sets` with `deploy/.` and `deploy/+n`, `get deploys`, `logs deploy`, `resources graph`, and `api`; verify CLI tests against a dev server cover every scenario in `cli` and the Score deploy scenarios in `deployment-sets-and-deltas`
- [ ] 6.4 Write `docs/cli.md` generated from clap help plus a developer quickstart; verify the quickstart commands run as written against a dev server

## 7. OpenTofu provider
- [ ] 7.1 Scaffold `provider/` as a Go module on terraform-plugin-framework with provider configuration from arguments and `OPENPORCH_*` env vars; verify `go test` passes and `tofu init` loads a locally built provider through a dev override
- [ ] 7.2 Implement resources `openporch_application`, `openporch_environment_type`, `openporch_environment`, `openporch_resource_type`, `openporch_resource_class`, `openporch_cloud_account`, `openporch_driver`, `openporch_value` with import; verify acceptance tests against a local server cover create, update, delete and import for each
- [ ] 7.3 Implement `openporch_resource_definition` (dynamic `driver_inputs`, sensitive secrets, key-set drift detection) and `openporch_resource_definition_criteria`; verify acceptance tests cover the definition-with-criteria scenario, the `$${}` escaping scenario and a no-op second plan
- [ ] 7.4 Generate provider docs with `tfplugindocs` and add a CI job for the Go module; verify docs are committed and the job is green

## 8. AWS reference platform and end-to-end (no cluster)
- [ ] 8.1 Write `examples/aws-platform/` in OpenTofu: env types, an `aws-role` cloud account, `postgres` on RDS, `s3` and `sqs` via the OpenTofu driver, `dns` via echo, with no Kubernetes definitions; verify `tofu validate` passes and a doc test applies it against a dev server
- [ ] 8.2 Write `examples/score-web/` with a Score file using postgres, s3 and sqs and an extension file selecting profile `openporch/none`; verify the in-memory e2e from 5.7 is repeated with these files
- [ ] 8.3 Add an opt-in e2e job (`just e2e-aws`) that applies the example platform against a real AWS account, runs `porch score deploy --wait`, asserts the RDS instance, bucket and queue exist and the workload outputs carry their connection details, rolls back with `+1`, and deletes the environment so `tofu destroy` removes everything; verify it passes once manually and its results are documented in `docs/e2e.md`
- [ ] 8.4 Write `docs/getting-started.md` covering server install, master key, state bucket, first token, provider setup and the first AWS-only deployment; verify a fresh reader can follow it on a clean machine using only the docs

## 9. Kubernetes workloads
- [ ] 9.1 Implement `openporch/k8s-cluster-static` and `openporch/k8s-cluster-eks` (describe cluster, presigned STS token) drivers; verify unit tests for the token against known vectors and a kind-based test for the static driver
- [ ] 9.2 Implement workload rendering for the `openporch/default` profile (Deployment, Service, ConfigMap, Secret, probes, resources, labels, extension file merges, `pod.*` field refs) and the implicit `k8s-cluster`/`k8s-namespace` graph nodes for that profile; verify golden-file tests cover the render scenario in `kubernetes-workloads` and graph tests show the nodes appear only for that profile
- [ ] 9.3 Implement namespace creation, manifest placement by `location`, server-side apply, readiness waiting with timeout, workload removal and namespace deletion; verify integration tests against kind cover apply, crash-loop failure, removal and environment deletion
- [ ] 9.4 Implement the runtime status endpoint; verify a kind test reports replica counts for a deployed workload
- [ ] 9.5 Extend `examples/aws-platform/` with EKS `k8s-cluster` and `k8s-namespace` definitions, switch `examples/score-web/` to profile `openporch/default`, and extend the e2e job (`just e2e-eks`) to check runtime status on the cluster; verify it passes once manually and `docs/e2e.md` is updated
- [ ] 9.6 Write `docs/kubernetes.md` describing cluster definitions for static and EKS clusters and the getting-started section for deploying to EKS; verify the EKS example validates against the server

## Workflow follow-up
- Archive the change once the milestone is implemented and the specs above are promoted to `openspec/specs/`.
- Open follow-up changes for the items listed as out of scope in the proposal (incremental mode, selectors, definition versions, remote runners, external secret stores, RBAC).
