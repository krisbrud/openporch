# CLAUDE.md

Mandatory rules. Detailed how-tos and templates live in
`.claude/skills/testing/SKILL.md` — invoke that skill when adding or
substantially changing tests.

## Development

- Target the Go toolchain in `go.mod` (currently `go 1.25`). `go build ./...`,
  `go vet ./...`, and `go vet -tags=integration ./...` must pass before
  committing. The integration-tagged vet catches breakage in tests that the
  default lane skips.
- Format with `gofmt` (tabs, standard imports). Don't add external deps without
  a clear reason — the stdlib + the modules already in `go.mod` cover most
  needs. Allowed test-only additions: `github.com/google/go-cmp/cmp` for
  diffs, `rsc.io/script/scripttest` for CLI tests.
- Keep packages under `internal/` cohesive: one responsibility per package,
  exported surface minimal. Wire dependencies via small interfaces (see
  `runner.Runner`, `store.Store`) so they can be faked in tests.
- Return errors with `fmt.Errorf("...: %w", err)`; never `panic` on user input
  or I/O.

## Testing — required outcomes

These are the properties every test must satisfy. The skill explains *how*.

1. **Defaults stay green and fast.** `go test -race -count=1 ./...` on a clean
   machine with only the Go toolchain must pass. No network, no ambient env,
   no current-working-dir assumptions, no `time.Sleep` for synchronization,
   no writes outside `t.TempDir()`. Set env via `t.Setenv`.
2. **Parallel-safe by default.** Every test calls `t.Parallel()` unless there
   is a documented reason not to (e.g. `t.Setenv` in a parent test).
3. **Failures are actionable.** A failing assertion must show the agent what
   changed, not just that something changed. Use `cmp.Diff` for non-trivial
   equality, and require it for any struct or slice comparison larger than
   two fields; use golden files for generated text (HCL, JSON, CLI output)
   with a `-update` flag to regenerate. Substring matches against generated
   output are not acceptable for new code.
4. **Every exported behavior in `internal/` and `api/` has a unit test** in
   the same package, file `<thing>_test.go`. Prefer table-driven tests when
   covering more than two cases of the same shape.
5. **CLI behavior is tested at the CLI.** Changes to `cmd/openporch/` must be
   covered by a testscript (`*.txtar`) case, not only by package-level tests.
6. **Integration tests cover the contract with the outside world.** They live
   next to the code under test and must:
   - start with `//go:build integration` so default `go test ./...` skips them;
   - probe their prereq (`docker`, `tofu`) and `t.Skipf` when missing locally,
     but **fail** when `CI=true` and the prereq is missing — silent skips in CI
     are a bug;
   - exercise the package's public API as a black-box (`package <pkg>_test`);
   - put all state under `t.TempDir()`, use non-default ports/names to avoid
     collisions, and register teardown with `t.Cleanup` that runs even on
     failure;
   - wrap the operation in `context.WithTimeout` and finish within the suite's
     `-timeout` ceiling;
   - build any image/module they need from `examples/` inside the test rather
     than depending on machine state.
7. **Assertion libraries are limited.** Allowed: stdlib `testing`, `go-cmp`,
   `scripttest`. Banned: `testify` (its `require` hides control flow) and any
   library that swallows the `*testing.T` API.

When adding a new external integration (a new runner, a new provider), add an
integration test alongside the unit tests; the unit tests cover logic, the
integration test covers the contract with the outside world.
