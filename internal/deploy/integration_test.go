//go:build integration

package deploy_test

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"testing"
	"time"

	"github.com/krbrudeli/openporch/internal/config"
	"github.com/krbrudeli/openporch/internal/deploy"
	"github.com/krbrudeli/openporch/internal/manifest"
	"github.com/krbrudeli/openporch/internal/runner"
	"github.com/krbrudeli/openporch/internal/store"
	"github.com/krbrudeli/openporch/internal/store/db"
)

// TestDeployFastAPIToLocalDocker runs the full pipeline against the host's
// docker daemon: builds the FastAPI image, deploys postgres + workload via
// openporch, asserts /health responds with {"db":"ok"}, then tears down.
//
// Run with: go test -tags=integration -timeout=5m ./internal/deploy/...
func TestDeployFastAPIToLocalDocker(t *testing.T) {
	if err := exec.Command("docker", "version").Run(); err != nil {
		t.Skipf("docker daemon not reachable: %v", err)
	}

	repoRoot := findRepoRoot(t)
	platformDir := filepath.Join(repoRoot, "examples", "platform")
	manifestPath := filepath.Join(repoRoot, "examples", "apps", "fastapi-demo", "manifest.yaml")
	appDir := filepath.Join(repoRoot, "examples", "apps", "fastapi-demo")

	// Build the image the manifest references.
	imageTag := "fastapi-demo:integration"
	if out, err := exec.Command("docker", "build", "-t", imageTag, appDir).CombinedOutput(); err != nil {
		t.Fatalf("docker build: %v\n%s", err, out)
	}

	cfg, err := config.Load(platformDir)
	if err != nil {
		t.Fatalf("load platform: %v", err)
	}
	m, err := manifest.Load(manifestPath)
	if err != nil {
		t.Fatalf("load manifest: %v", err)
	}
	// Override the image to the just-built tag and use a non-default port to
	// avoid colliding with whatever the developer has running on 8080/5433.
	m.Workloads["api"].Params["image"] = imageTag
	m.Workloads["api"].Params["host_port"] = 18080
	dbResource := m.Workloads["api"].Resources["db"]
	if dbResource.Params == nil {
		dbResource.Params = map[string]any{}
	}
	dbResource.Params["host_port"] = 15433
	m.Workloads["api"].Resources["db"] = dbResource

	stateRoot := t.TempDir()
	const project = "integration"
	const env = "test"

	s := &store.FS{Root: stateRoot}
	r := &runner.LocalTofu{
		BinaryPath:     "",
		PluginCacheDir: filepath.Join(stateRoot, "plugin-cache"),
	}
	opts := deploy.Options{
		Manifest: m, Platform: cfg, Store: s, Runner: r,
		ProjectID: project, EnvID: env, EnvTypeID: "local",
	}

	ctx, cancel := context.WithTimeout(context.Background(), 4*time.Minute)
	defer cancel()

	// Always tear down, even if the test fails partway. We do this with
	// t.Cleanup before calling Run so that a panic during Run still triggers
	// destroy.
	t.Cleanup(func() {
		dctx, dcancel := context.WithTimeout(context.Background(), 2*time.Minute)
		defer dcancel()
		if _, err := deploy.Destroy(dctx, deploy.DestroyOptions{Options: opts, Prune: true}); err != nil {
			t.Logf("cleanup destroy: %v", err)
		}
	})

	res, err := deploy.Run(ctx, opts)
	if err != nil {
		t.Fatalf("deploy.Run: %v", err)
	}
	if got := res.Outputs["api_url"]; got != "http://localhost:18080" {
		t.Errorf("api_url = %q, want http://localhost:18080", got)
	}

	// Poll /health: the container needs a moment to start uvicorn and for
	// postgres to accept connections through the host.docker.internal hop.
	deadline := time.Now().Add(60 * time.Second)
	var lastErr error
	for time.Now().Before(deadline) {
		body, status, err := httpGet("http://localhost:18080/health")
		if err == nil && status == 200 {
			var got map[string]string
			if err := json.Unmarshal(body, &got); err == nil && got["db"] == "ok" {
				return // success
			}
			lastErr = err
			t.Logf("/health body=%s parse-err=%v", string(body), err)
		} else {
			lastErr = err
		}
		time.Sleep(2 * time.Second)
	}
	t.Fatalf("/health never returned db=ok: lastErr=%v", lastErr)
}

// TestPlanOnlyFastAPIDemo runs `--plan-only` against the fastapi-demo manifest
// and asserts:
//   - no Docker containers were started by the run (plan does not apply);
//   - the deployment row exists with mode=plan_only and status=planned;
//   - every resource row has status=planned and a non-empty plan_path that
//     points at a file on disk;
//   - re-running plan-only against the same state produces a new deployment
//     row, not a duplicate apply.
//
// Run with: go test -tags=integration -timeout=10m ./internal/deploy/...
func TestPlanOnlyFastAPIDemo(t *testing.T) {
	if err := exec.Command("tofu", "version").Run(); err != nil {
		t.Skipf("tofu binary not available: %v", err)
	}
	if err := exec.Command("docker", "version").Run(); err != nil {
		t.Skipf("docker daemon not reachable: %v", err)
	}

	repoRoot := findRepoRoot(t)
	platformDir := filepath.Join(repoRoot, "examples", "platform")
	manifestPath := filepath.Join(repoRoot, "examples", "apps", "fastapi-demo", "manifest.yaml")
	appDir := filepath.Join(repoRoot, "examples", "apps", "fastapi-demo")

	// kreuzwerker/docker's docker_image resource inspects the image during
	// plan; the plan fails if the image isn't present locally. Build it the
	// same way TestDeployFastAPIToLocalDocker does.
	imageTag := "fastapi-demo:plan-integration"
	if out, err := exec.Command("docker", "build", "-t", imageTag, appDir).CombinedOutput(); err != nil {
		t.Fatalf("docker build: %v\n%s", err, out)
	}

	cfg, err := config.Load(platformDir)
	if err != nil {
		t.Fatalf("load platform: %v", err)
	}
	m, err := manifest.Load(manifestPath)
	if err != nil {
		t.Fatalf("load manifest: %v", err)
	}
	// Override the image to the just-built tag and use non-default ports so
	// plan-only doesn't collide with whatever the developer has running.
	m.Workloads["api"].Params["image"] = imageTag
	m.Workloads["api"].Params["host_port"] = 18091
	// Replace the cross-resource DATABASE_URL placeholder with a literal: in
	// plan-only the upstream `db` resource is never applied, so its outputs
	// are not available. Leaving the unresolved `${...}` would surface as a
	// Terraform interpolation error during `tofu plan`. The actual integration
	// of cross-resource resolution against persisted plan state is out of
	// scope for v0 plan-only.
	m.Workloads["api"].Params["env"] = map[string]any{
		"DATABASE_URL": "postgresql://placeholder:5432/db",
	}
	dbResource := m.Workloads["api"].Resources["db"]
	if dbResource.Params == nil {
		dbResource.Params = map[string]any{}
	}
	dbResource.Params["host_port"] = 15491
	m.Workloads["api"].Resources["db"] = dbResource

	stateRoot := t.TempDir()
	const project = "plan-integration"
	const env = "test"

	s := &store.FS{Root: stateRoot}
	r := &runner.LocalTofu{
		PluginCacheDir: filepath.Join(stateRoot, "plugin-cache"),
	}
	openDB, err := db.Open(stateRoot)
	if err != nil {
		t.Fatalf("db.Open: %v", err)
	}
	t.Cleanup(func() { openDB.Close() })

	// fastapi-demo workloads/resources end up as docker containers named
	// `openporch-<name>` per workload-local-docker module. Plan must not
	// create any of them.
	mustNoContainers := func(when string) {
		for _, name := range []string{"openporch-api", "openporch-db"} {
			out, err := exec.Command("docker", "ps", "-aq", "--filter", "name=^"+name+"$").CombinedOutput()
			if err != nil {
				t.Fatalf("docker ps (%s, %s): %v\n%s", when, name, err, out)
			}
			if len(out) > 0 {
				t.Errorf("expected no container %q (%s), got id(s): %s", name, when, string(out))
			}
		}
	}

	runPlanOnly := func(deploymentID string) {
		t.Helper()
		opts := deploy.Options{
			Manifest: m, Platform: cfg, Store: s, Runner: r, RunnerID: "local-tofu",
			ProjectID: project, EnvID: env, EnvTypeID: "local",
			Recorder:     db.NewRecorder(openDB),
			PlanOnly:     true,
			DeploymentID: deploymentID,
		}
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Minute)
		defer cancel()
		if _, err := deploy.Run(ctx, opts); err != nil {
			t.Fatalf("deploy.Run plan-only %s: %v", deploymentID, err)
		}
	}

	mustNoContainers("before plan")
	runPlanOnly("plan-1")
	mustNoContainers("after plan-1")

	rdr := db.NewReader(openDB)
	det, err := rdr.GetDeployment(context.Background(), "plan-1")
	if err != nil {
		t.Fatalf("GetDeployment plan-1: %v", err)
	}
	if det == nil {
		t.Fatal("plan-1 not found in DB")
	}
	if det.Mode != "plan_only" {
		t.Errorf("Mode = %q, want plan_only", det.Mode)
	}
	if det.Status != "planned" {
		t.Errorf("Status = %q, want planned", det.Status)
	}
	if len(det.Resources) == 0 {
		t.Fatal("no resource rows recorded")
	}
	for _, row := range det.Resources {
		if row.Status != "planned" {
			t.Errorf("resource %q: Status = %q, want planned", row.ResourceKey, row.Status)
		}
		if row.PlanPath == "" {
			t.Errorf("resource %q: PlanPath is empty", row.ResourceKey)
			continue
		}
		if _, err := os.Stat(row.PlanPath); err != nil {
			t.Errorf("resource %q: plan file missing: %v", row.ResourceKey, err)
		}
	}

	// Re-run plan-only: a fresh deployment row, no apply, plans regenerate.
	runPlanOnly("plan-2")
	mustNoContainers("after plan-2")

	det2, err := rdr.GetDeployment(context.Background(), "plan-2")
	if err != nil {
		t.Fatalf("GetDeployment plan-2: %v", err)
	}
	if det2 == nil || det2.Status != "planned" {
		t.Errorf("plan-2 detail = %+v, want status=planned", det2)
	}
	rows, err := rdr.ListDeployments(context.Background(), project, env, 10)
	if err != nil {
		t.Fatalf("ListDeployments: %v", err)
	}
	if len(rows) < 2 {
		t.Errorf("expected >=2 deployments after rerun, got %d", len(rows))
	}
	for _, row := range rows {
		if row.Mode != "plan_only" {
			t.Errorf("deployment %s: Mode = %q, want plan_only (no accidental apply)", row.ID, row.Mode)
		}
	}
}

func httpGet(url string) ([]byte, int, error) {
	resp, err := http.Get(url)
	if err != nil {
		return nil, 0, err
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(resp.Body)
	return body, resp.StatusCode, err
}

func findRepoRoot(t *testing.T) string {
	_, file, _, ok := runtime.Caller(0)
	if !ok {
		t.Fatal("runtime.Caller failed")
	}
	// this file lives at <root>/internal/deploy/integration_test.go
	return filepath.Clean(filepath.Join(filepath.Dir(file), "..", ".."))
}
