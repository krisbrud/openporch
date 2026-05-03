---
name: testing
description: Playbook for writing unit, CLI, and integration tests in the openporch repo. Invoke when adding new tests, debugging flaky tests, or reviewing test changes. Covers fakes, golden files, testscript, and the integration-test build-tag pattern.
---

# Testing playbook

Companion to the rules in `CLAUDE.md`. The rules in `CLAUDE.md` say *what
must be true* of the test suite; this skill says *how to write a test that
satisfies them*.

Read `CLAUDE.md` first if you haven't. Then pick the section below that
matches the layer you're testing.

## Choosing a layer

| What you're changing                                  | Test layer                                |
|-------------------------------------------------------|-------------------------------------------|
| Pure logic in `internal/<pkg>/` or `api/v1alpha1/`    | Unit test in the same package            |
| `cmd/openporch/` flag, exit code, or output           | testscript (`cmd/openporch/testdata/*.txtar`) |
| Real `tofu` / Docker behavior                         | Integration test (build tag `integration`)|

If a change spans layers, add tests at every layer it crosses. Don't rely on
the integration test alone to catch a unit-level regression — its feedback
loop is too slow.

## Unit tests

Templates and conventions:

- File name: `<thing>_test.go`, same package as `<thing>.go`. Use the
  `<pkg>_test` (black-box) variant only when you genuinely want to test the
  public API surface (most existing tests use white-box; that's fine).
- Top of every test: `t.Parallel()`.
- Table-driven shape (use when ≥ 3 cases of the same shape):

  ```go
  func TestModule_specificity(t *testing.T) {
      t.Parallel()
      tests := []struct {
          name string
          ctx  Context
          want string
      }{
          {"catchall", Context{ResourceType: "postgres"}, "default"},
          // ...
      }
      for _, tc := range tests {
          tc := tc
          t.Run(tc.name, func(t *testing.T) {
              t.Parallel()
              got, err := Module(rules, tc.ctx)
              if err != nil {
                  t.Fatalf("Module: %v", err)
              }
              if diff := cmp.Diff(tc.want, got); diff != "" {
                  t.Errorf("Module() mismatch (-want +got):\n%s", diff)
              }
          })
      }
  }
  ```

- `t.Fatalf` for setup/precondition failures (further assertions would be
  meaningless). `t.Errorf` for independent assertions where you want the
  rest of the test to keep going.
- Filesystem state: `t.TempDir()` only. Helpers that take `*testing.T` start
  with `t.Helper()`.
- Env vars: `t.Setenv` (which forces serial execution within that test —
  acceptable trade-off but call out the lack of `t.Parallel`).

### Fakes vs real implementations

Prefer in-memory fakes for unit tests; reserve real implementations
(`store.FS`, `runner.LocalTofu`) for integration tests.

If a fake doesn't exist for an interface you're testing against, add one
under `internal/<pkg>/<pkg>test/` (e.g. `storetest.Fake`, `runnertest.Recording`).
Keep the fake minimal: just enough to satisfy the interface and let the test
assert against recorded calls.

A fake should:

1. Implement the full interface so calling code compiles against either.
2. Record calls in fields the test can read (`Calls []ApplyCall`).
3. Allow stubbed return values via fields (`ApplyErr error`).

### Diffs over equality

Use `github.com/google/go-cmp/cmp` for any struct or slice comparison larger
than two fields. Write the diff message as `"X mismatch (-want +got):\n%s"`
so the agent reading the failure can immediately see which field changed.

For unexported fields, use `cmp.AllowUnexported(MyType{})` rather than
exporting fields just for tests.

## CLI tests (testscript)

`cmd/openporch/` is a public surface. Cover it with `rsc.io/script/scripttest`:

```go
// cmd/openporch/cli_test.go
//go:build !integration

package main

import (
    "os"
    "testing"

    "rsc.io/script/scripttest"
)

func TestMain(m *testing.M) {
    os.Exit(scripttest.RunMain(m, map[string]func() int{
        "openporch": run, // run is your main-equivalent that returns an int exit code
    }))
}

func TestCLI(t *testing.T) {
    t.Parallel()
    scripttest.Run(t, scripttest.DefaultCmds(), nil, "testdata/*.txtar")
}
```

Each `.txtar` file is a self-contained scenario:

```
# validate accepts a well-formed manifest
exec openporch validate manifest.yaml --platform platform/
stdout 'manifest valid'
! stderr .

-- manifest.yaml --
apiVersion: openporch/v1alpha1
kind: Application
...

-- platform/modules.yaml --
...
```

Why testscript: failures point at a specific line in a text file the agent
can edit directly, and the scenario reads top-to-bottom like a shell session.
Far more debuggable than building a `cobra.Command` by hand inside a `_test.go`.

## Golden files for generated text

For anything that produces text we care about byte-for-byte (rendered HCL in
`internal/tofu`, JSON output, error messages), use golden files:

```go
func TestRender_smoke(t *testing.T) {
    t.Parallel()
    got, err := Render(plan)
    if err != nil {
        t.Fatalf("Render: %v", err)
    }
    goldenPath := filepath.Join("testdata", "render_smoke.golden.hcl")
    if *update {
        if err := os.WriteFile(goldenPath, []byte(got), 0o644); err != nil {
            t.Fatal(err)
        }
    }
    want, err := os.ReadFile(goldenPath)
    if err != nil {
        t.Fatal(err)
    }
    if diff := cmp.Diff(string(want), got); diff != "" {
        t.Errorf("rendered output mismatch (-want +got):\n%s", diff)
    }
}

var update = flag.Bool("update", false, "update golden files")
```

Regenerate with `go test ./internal/tofu -update`. Commit the golden file.
Reviewers diff the golden alongside the code change — that's the point.

Don't combine "golden file" with "substring assertion": the golden file is
the test. If a substring is what you actually care about (e.g. "the rendered
output names the user-supplied image"), that's a separate, named test.

## Integration tests

Location: same package as the code, file `<feature>_integration_test.go`,
build tag `integration`. Run with:

```
go test -tags=integration -timeout=5m ./...
```

Skeleton (cribbed from `internal/deploy/integration_test.go`):

```go
//go:build integration

package deploy_test

import (
    "context"
    "testing"
    "time"

    "github.com/krbrudeli/openporch/internal/integrationtest"
)

func TestDeploy_endToEnd(t *testing.T) {
    integrationtest.RequireDocker(t)
    integrationtest.RequireTofu(t)
    // Do NOT t.Parallel() integration tests that bind host resources.

    ctx, cancel := context.WithTimeout(context.Background(), 4*time.Minute)
    defer cancel()

    stateRoot := t.TempDir()
    // ... build manifest, override host_port to a non-default value,
    //     wire store.FS{Root: stateRoot} + runner.LocalTofu{...}.

    t.Cleanup(func() {
        // Always tear down. Use a fresh, short context so cleanup runs even
        // if the test's main ctx was cancelled.
        cleanupCtx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
        defer cancel()
        _ = deploy.Destroy(cleanupCtx, opts)
    })

    if err := deploy.Run(ctx, opts); err != nil {
        t.Fatalf("Run: %v", err)
    }
    // Assert against the real outside world (HTTP probe, docker ps, ...).
}
```

Critical points:

- **Prerequisites fail, never skip.** Integration tests are already guarded
  behind `-tags=integration`, so missing Docker/OpenTofu means the opted-in
  test environment is broken. Use
  `internal/integrationtest.RequireDocker` and
  `internal/integrationtest.RequireTofu` rather than inlining prerequisite
  checks or calling `t.Skipf`.
- **Non-default ports/names.** Pick something unlikely to collide with a
  developer's running stack. `host_port: 18080` and `host_port: 15433` in
  `internal/deploy/integration_test.go` are the existing convention.
- **Teardown uses a fresh context.** If the main `ctx` was cancelled or hit
  its timeout, you still need `t.Cleanup` to actually run `destroy`.
- **No `t.Parallel()` for tests that bind ports** or share a Docker daemon
  namespace. Multiple integration tests should serialize unless they pick
  disjoint ports/names.
- **Build artifacts from `examples/`** inside the test (`docker build -t ...`).
  Don't assume an image is already on the machine.

When adding a new external integration (a new runner, a new provider),
template a new integration test from the deploy one and keep any new
CI-sensitive prerequisite checks in `internal/integrationtest/`.

## Anti-patterns to flag in review

- `reflect.DeepEqual(...)` followed by `t.Errorf("got %+v want %+v", a, b)`
  on anything bigger than a 2-field struct. Replace with `cmp.Diff`.
- Substring assertions against generated text. Replace with a golden file.
- Tests that read or write outside `t.TempDir()`.
- Tests that depend on `examples/` paths via `findRepoRoot`-style walks
  outside of the dedicated end-to-end integration test. Use
  `internal/<pkg>/testdata/` instead.
- `time.Sleep` for synchronization. Use channels, `context`, or polling with
  a deadline.
- Integration tests that `t.Skipf` for missing prerequisites — the build tag is
  the opt-in guard, so missing prerequisites should fail.
