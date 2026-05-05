package main

import (
	"bytes"
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/spf13/cobra"

	"github.com/krbrudeli/openporch/internal/deploy"
	"github.com/krbrudeli/openporch/internal/store/db"
)

// runDeploy executes "openporch deploy <args...> --platform <p> --state-root <s>
// --dry-run" so the pipeline runs through render but skips OpenTofu.
func runDeploy(t *testing.T, stateRoot, platform string, args ...string) (string, error) {
	t.Helper()
	root := &cobra.Command{Use: "openporch", SilenceUsage: true, SilenceErrors: true}
	root.AddCommand(newDeployCmd())
	var buf bytes.Buffer
	root.SetOut(&buf)
	root.SetErr(&buf)
	allArgs := append([]string{"deploy"}, args...)
	allArgs = append(allArgs, "--state-root", stateRoot, "--platform", platform, "--dry-run")
	root.SetArgs(allArgs)
	err := root.Execute()
	return buf.String(), err
}

// writeDeployPlatform creates a minimum platform on disk wiring one inline
// "workload" module via a catch-all rule.
func writeDeployPlatform(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	platform := `
apiVersion: openporch/v1alpha1
kind: ResourceType
id: workload
---
apiVersion: openporch/v1alpha1
kind: Module
id: workload-stub
resource_type: workload
module_source: inline
module_source_code: |
  output "ok" { value = "ok" }
---
apiVersion: openporch/v1alpha1
kind: ModuleRule
id: catchall
resource_type: workload
module_id: workload-stub
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
	if err := os.WriteFile(filepath.Join(dir, "platform.yaml"), []byte(platform), 0o644); err != nil {
		t.Fatalf("write platform: %v", err)
	}
	return dir
}

const deployManifest = `apiVersion: openporch/v1alpha1
kind: Application
metadata:
  name: test-app
  project: myproj
workloads:
  api:
    type: workload
`

// seedDeployForCmd writes one finished deployment for (project, env) into the
// SQLite store and returns its ID and the manifest YAML used.
func seedDeployForCmd(t *testing.T, stateRoot, project, env, id string, started time.Time) {
	t.Helper()
	d, err := db.Open(stateRoot)
	if err != nil {
		t.Fatalf("db.Open: %v", err)
	}
	defer d.Close()
	rec := db.NewRecorder(d)
	ctx := context.Background()
	if err := rec.StartDeployment(ctx, deploy.DeploymentRecord{
		ID: id, Project: project, Env: env, EnvType: "local", Mode: "deploy",
		StartedAt: started, ManifestYAML: deployManifest, GraphJSON: `{}`,
	}); err != nil {
		t.Fatalf("StartDeployment: %v", err)
	}
	if err := rec.FinishDeployment(ctx, id, "succeeded", started.Add(time.Minute)); err != nil {
		t.Fatalf("FinishDeployment: %v", err)
	}
}

// ---------------------------------------------------------------------------
// File source (regression: existing path still works)
// ---------------------------------------------------------------------------

func TestDeploy_FileSource(t *testing.T) {
	t.Parallel()
	platform := writeDeployPlatform(t)
	stateRoot := t.TempDir()
	mfDir := t.TempDir()
	mfPath := filepath.Join(mfDir, "manifest.yaml")
	if err := os.WriteFile(mfPath, []byte(deployManifest), 0o644); err != nil {
		t.Fatalf("write manifest: %v", err)
	}

	out, err := runDeploy(t, stateRoot, platform, mfPath)
	if err != nil {
		t.Fatalf("runDeploy: %v\n%s", err, out)
	}
	if !strings.Contains(out, "Dry-run complete") {
		t.Errorf("expected dry-run output, got:\n%s", out)
	}
	if !strings.Contains(out, "module=workload-stub") {
		t.Errorf("expected module-stub line, got:\n%s", out)
	}
}

// ---------------------------------------------------------------------------
// deployment://HEAD
// ---------------------------------------------------------------------------

func TestDeploy_DeploymentHEAD(t *testing.T) {
	t.Parallel()
	platform := writeDeployPlatform(t)
	stateRoot := t.TempDir()
	base := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
	seedDeployForCmd(t, stateRoot, "myproj", "dev", "first", base)
	seedDeployForCmd(t, stateRoot, "myproj", "dev", "head-target", base.Add(time.Hour))

	out, err := runDeploy(t, stateRoot, platform, "deployment://HEAD",
		"--project", "myproj", "--env", "dev")
	if err != nil {
		t.Fatalf("runDeploy: %v\n%s", err, out)
	}
	if !strings.Contains(out, "Dry-run complete") {
		t.Errorf("expected dry-run output, got:\n%s", out)
	}
}

func TestDeploy_DeploymentHEAD_RequiresProjectAndEnv(t *testing.T) {
	t.Parallel()
	platform := writeDeployPlatform(t)
	stateRoot := t.TempDir()
	// --env defaults to "default"; project is empty so HEAD must reject.
	_, err := runDeploy(t, stateRoot, platform, "deployment://HEAD")
	if err == nil {
		t.Fatal("expected error when project unset, got nil")
	}
	if !strings.Contains(err.Error(), "deployment://HEAD requires --project and --env") {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestDeploy_DeploymentHEAD_NoSuccessful(t *testing.T) {
	t.Parallel()
	platform := writeDeployPlatform(t)
	stateRoot := t.TempDir()
	_, err := runDeploy(t, stateRoot, platform, "deployment://HEAD",
		"--project", "myproj", "--env", "dev")
	if err == nil {
		t.Fatal("expected error when no deployments exist, got nil")
	}
	if !strings.Contains(err.Error(), `no successful deployment found for project="myproj" env="dev"`) {
		t.Fatalf("unexpected error: %v", err)
	}
}

// ---------------------------------------------------------------------------
// deployment://<id>
// ---------------------------------------------------------------------------

func TestDeploy_DeploymentByID(t *testing.T) {
	t.Parallel()
	platform := writeDeployPlatform(t)
	stateRoot := t.TempDir()
	seedDeployForCmd(t, stateRoot, "myproj", "dev", "dep-target",
		time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC))

	// --env left at default; the loaded manifest sets project itself.
	out, err := runDeploy(t, stateRoot, platform, "deployment://dep-target")
	if err != nil {
		t.Fatalf("runDeploy: %v\n%s", err, out)
	}
	if !strings.Contains(out, "Dry-run complete") {
		t.Errorf("expected dry-run output, got:\n%s", out)
	}
}

func TestDeploy_DeploymentByID_NotFound(t *testing.T) {
	t.Parallel()
	platform := writeDeployPlatform(t)
	stateRoot := t.TempDir()
	_, err := runDeploy(t, stateRoot, platform, "deployment://missing")
	if err == nil {
		t.Fatal("expected error, got nil")
	}
	if !strings.Contains(err.Error(), `deployment "missing" not found`) {
		t.Fatalf("unexpected error: %v", err)
	}
}

// ---------------------------------------------------------------------------
// environment://<env>
// ---------------------------------------------------------------------------

func TestDeploy_EnvironmentSource(t *testing.T) {
	t.Parallel()
	platform := writeDeployPlatform(t)
	stateRoot := t.TempDir()
	base := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
	seedDeployForCmd(t, stateRoot, "myproj", "staging", "stg-1", base)

	out, err := runDeploy(t, stateRoot, platform, "environment://staging",
		"--project", "myproj", "--env", "prod")
	if err != nil {
		t.Fatalf("runDeploy: %v\n%s", err, out)
	}
	if !strings.Contains(out, "Dry-run complete") {
		t.Errorf("expected dry-run output, got:\n%s", out)
	}
}

func TestDeploy_EnvironmentSource_NoSuccessful(t *testing.T) {
	t.Parallel()
	platform := writeDeployPlatform(t)
	stateRoot := t.TempDir()
	_, err := runDeploy(t, stateRoot, platform, "environment://staging",
		"--project", "myproj", "--env", "prod")
	if err == nil {
		t.Fatal("expected error, got nil")
	}
	if !strings.Contains(err.Error(), `no successful deployment found for project="myproj" env="staging"`) {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestDeploy_UnknownScheme(t *testing.T) {
	t.Parallel()
	platform := writeDeployPlatform(t)
	stateRoot := t.TempDir()
	_, err := runDeploy(t, stateRoot, platform, "s3://bucket/manifest.yaml")
	if err == nil {
		t.Fatal("expected error, got nil")
	}
	if !strings.Contains(err.Error(), `unknown manifest source scheme "s3"`) {
		t.Fatalf("unexpected error: %v", err)
	}
}
