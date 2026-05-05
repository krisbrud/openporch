//go:build integration

package main

import (
	"context"
	"os"
	"path/filepath"
	"testing"
	"time"

	v1 "github.com/krbrudeli/openporch/api/v1alpha1"
	"github.com/krbrudeli/openporch/internal/config"
	"github.com/krbrudeli/openporch/internal/deploy"
	"github.com/krbrudeli/openporch/internal/integrationtest"
	"github.com/krbrudeli/openporch/internal/manifest"
	"github.com/krbrudeli/openporch/internal/runner"
	"github.com/krbrudeli/openporch/internal/store"
	"github.com/krbrudeli/openporch/internal/store/db"
)

// integrationPlatformYAML wires a single workload type to an inline OpenTofu
// module that creates no real resources — just a string output. We can run a
// full tofu init/apply cycle quickly without touching Docker.
const integrationPlatformYAML = `
apiVersion: openporch/v1alpha1
kind: ResourceType
id: workload
output_schema:
  type: object
  properties:
    name: {type: string}
---
apiVersion: openporch/v1alpha1
kind: Module
id: workload-noop
resource_type: workload
module_source: inline
module_source_code: |
  variable "name" { type = string, default = "noop" }
  output "name" { value = var.name }
---
apiVersion: openporch/v1alpha1
kind: ModuleRule
id: catchall
resource_type: workload
module_id: workload-noop
---
apiVersion: openporch/v1alpha1
kind: Runner
id: local-tofu
type: local-tofu
---
apiVersion: openporch/v1alpha1
kind: RunnerRule
id: catchall-runner
runner_id: local-tofu
`

const integrationManifestYAML = `apiVersion: openporch/v1alpha1
kind: Application
metadata:
  name: redeploy-app
  project: redeploy-proj
workloads:
  api:
    type: workload
`

// TestDeployRedeployFromHEAD covers issue #34: after an initial successful
// deploy from a YAML file, re-deploying from "deployment://HEAD" must load
// the manifest from SQLite history and produce an identical applied state.
//
// Run with: go test -tags=integration -timeout=5m ./cmd/openporch/...
func TestDeployRedeployFromHEAD(t *testing.T) {
	integrationtest.RequireTofu(t)

	platformDir := t.TempDir()
	if err := os.WriteFile(filepath.Join(platformDir, "platform.yaml"),
		[]byte(integrationPlatformYAML), 0o644); err != nil {
		t.Fatalf("write platform: %v", err)
	}
	manifestPath := filepath.Join(t.TempDir(), "manifest.yaml")
	if err := os.WriteFile(manifestPath, []byte(integrationManifestYAML), 0o644); err != nil {
		t.Fatalf("write manifest: %v", err)
	}

	cfg, err := config.Load(platformDir)
	if err != nil {
		t.Fatalf("load platform: %v", err)
	}

	stateRoot := t.TempDir()
	openDB, err := db.Open(stateRoot)
	if err != nil {
		t.Fatalf("db.Open: %v", err)
	}
	t.Cleanup(func() { openDB.Close() })

	const project = "redeploy-proj"
	const env = "dev"
	r := &runner.LocalTofu{PluginCacheDir: filepath.Join(stateRoot, "plugin-cache")}
	s := &store.FS{Root: stateRoot}

	doApply := func(ctx context.Context, m *v1.Manifest, deploymentID string) {
		t.Helper()
		opts := deploy.Options{
			Manifest: m, Platform: cfg, Store: s, Runner: r, RunnerID: "local-tofu",
			ProjectID: project, EnvID: env, EnvTypeID: "local",
			Recorder:     db.NewRecorder(openDB),
			DeploymentID: deploymentID,
		}
		if _, err := deploy.Run(ctx, opts); err != nil {
			t.Fatalf("deploy.Run %s: %v", deploymentID, err)
		}
	}

	// 1) Initial deploy from YAML file — should produce a recorded deployment.
	mf, err := manifest.Load(manifestPath)
	if err != nil {
		t.Fatalf("manifest.Load: %v", err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 4*time.Minute)
	defer cancel()
	doApply(ctx, mf, "first")

	// 2) Resolve `deployment://HEAD` against the recorded history; the
	// resolver is the function under test for this issue.
	src, err := parseManifestSource("deployment://HEAD")
	if err != nil {
		t.Fatalf("parseManifestSource: %v", err)
	}
	loaded, err := resolveManifestSource(ctx, db.NewReader(openDB), src, project, env)
	if err != nil {
		t.Fatalf("resolveManifestSource HEAD: %v", err)
	}
	if loaded.Metadata.Name != "redeploy-app" {
		t.Fatalf("HEAD manifest Name = %q, want redeploy-app", loaded.Metadata.Name)
	}
	doApply(ctx, loaded, "from-head")

	// 3) Both deployments should be present and succeeded; HEAD now points
	// at "from-head".
	rdr := db.NewReader(openDB)
	det, err := rdr.GetLastSuccessfulDeployment(ctx, project, env)
	if err != nil {
		t.Fatalf("GetLastSuccessfulDeployment: %v", err)
	}
	if det == nil {
		t.Fatal("no successful deployment found after redeploy")
	}
	if det.ID != "from-head" {
		t.Errorf("HEAD deployment ID = %q, want from-head", det.ID)
	}
	if det.Status != "succeeded" {
		t.Errorf("HEAD status = %q, want succeeded", det.Status)
	}

	rows, err := rdr.ListDeployments(ctx, project, env, 10)
	if err != nil {
		t.Fatalf("ListDeployments: %v", err)
	}
	if len(rows) != 2 {
		t.Fatalf("expected 2 deployments, got %d", len(rows))
	}

	// 4) Promote the same manifest into a new "prod" environment via
	// environment://dev. This validates the cross-environment source path.
	src, err = parseManifestSource("environment://dev")
	if err != nil {
		t.Fatalf("parseManifestSource: %v", err)
	}
	promoted, err := resolveManifestSource(ctx, rdr, src, project, "prod")
	if err != nil {
		t.Fatalf("resolveManifestSource environment://dev: %v", err)
	}
	if promoted.Metadata.Name != "redeploy-app" {
		t.Errorf("promoted Name = %q, want redeploy-app", promoted.Metadata.Name)
	}
	// Apply to prod and confirm a new succeeded row appears for that env.
	prodOpts := deploy.Options{
		Manifest: promoted, Platform: cfg, Store: s, Runner: r, RunnerID: "local-tofu",
		ProjectID: project, EnvID: "prod", EnvTypeID: "local",
		Recorder: db.NewRecorder(openDB), DeploymentID: "prod-1",
	}
	if _, err := deploy.Run(ctx, prodOpts); err != nil {
		t.Fatalf("deploy.Run prod: %v", err)
	}
	prodHEAD, err := rdr.GetLastSuccessfulDeployment(ctx, project, "prod")
	if err != nil {
		t.Fatalf("prod HEAD: %v", err)
	}
	if prodHEAD == nil || prodHEAD.ID != "prod-1" {
		t.Errorf("prod HEAD = %+v, want id=prod-1", prodHEAD)
	}
}
