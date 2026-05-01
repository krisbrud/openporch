//go:build integration

package deploy_test

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
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
