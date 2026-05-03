# CLAUDE.md

Mandatory practices for development and testing in this repo.

## Development

- Target the Go toolchain in `go.mod` (currently `go 1.25`). Run `go build ./...` and `go vet ./...` before committing.
- Format with `gofmt` (tabs, standard imports). No new external deps without a clear reason — the stdlib + the modules already in `go.mod` cover most needs.
- Keep packages under `internal/` cohesive: one responsibility per package, exported surface minimal. Wire dependencies via small interfaces (see `runner.Runner`, `store.Store`) so they can be faked in tests.
- Return errors with `fmt.Errorf("...: %w", err)`; never `panic` on user input or I/O.

## Unit tests (mandatory)

- Every exported function or behavior in `internal/` and `api/` must have a unit test in the same package, file `<thing>_test.go`.
- Use the standard library `testing` package only — **do not** add `testify`, `gomega`, or other assertion libs. Use `t.Fatalf` for setup/precondition failures and `t.Errorf` to keep going on independent assertions.
- Prefer table-driven tests (`tests := []struct{...}{...}` + `t.Run(tc.name, ...)`) when covering more than two cases of the same shape.
- Put filesystem state under `t.TempDir()`. Mark helpers with `t.Helper()`. Never read or write outside the temp dir or the repo's own `examples/`.
- Tests must be hermetic and parallel-safe: no network, no `time.Sleep` for synchronization, no reliance on `$HOME`, ambient env, or current working directory. Set env via `t.Setenv`.
- Default `go test ./...` must pass on a clean machine with only the Go toolchain installed.

## Integration tests (mandatory rules)

Integration tests cover real behavior against external systems we depend on: the OpenTofu CLI (`tofu`), Docker, and the on-disk state layout end-to-end. They live next to the code under test and follow these rules:

1. **Build tag.** First line is `//go:build integration`. They never run under the default `go test ./...`. Run with `go test -tags=integration -timeout=5m ./...`.
2. **Skip, don't fail, when prerequisites are missing.** Probe the dependency at the top of the test (e.g. `exec.Command("docker", "version").Run()`, `exec.LookPath("tofu")`) and call `t.Skipf` if absent. CI without Docker must still go green on `-tags=integration` for unrelated suites.
3. **Black-box package.** Use `package <pkg>_test` and exercise the public API (`deploy.Run`, `store.FS`, `runner.LocalTofu`) — not internals. The integration test should resemble how `cmd/openporch` uses the package.
4. **Isolated state.** Always use `t.TempDir()` as the state root. Never write to `.openporch/` in the repo. Pick non-default ports/names (see `host_port: 18080` in `internal/deploy/integration_test.go`) so the test doesn't collide with a developer's running stack.
5. **Always tear down.** Register cleanup with `t.Cleanup` (or `defer`) that runs `destroy` even when the test fails partway. Leaking Docker containers or `tofu` state breaks the next run.
6. **Bounded.** Wrap the operation in `context.WithTimeout`; the whole test must finish within `-timeout=5m`. Long-running suites get their own `_slow_test.go` behind an additional tag.
7. **Real artifacts only.** If the test needs an image/module, build it from `examples/` inside the test — don't depend on something that "should already be on the machine".

When adding a new external integration (a new runner, a new provider), add an integration test alongside the unit tests; the unit tests cover logic, the integration test covers the contract with the outside world.
