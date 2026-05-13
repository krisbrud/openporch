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

// runRollback executes "openporch rollback <args...> --platform <p>
// --state-root <s> --dry-run" so the pipeline runs through render but skips
// OpenTofu.
func runRollback(t *testing.T, stateRoot, platform string, args ...string) (string, error) {
	t.Helper()
	root := &cobra.Command{Use: "openporch", SilenceUsage: true, SilenceErrors: true}
	root.AddCommand(newRollbackCmd())
	var buf bytes.Buffer
	root.SetOut(&buf)
	root.SetErr(&buf)
	allArgs := append([]string{"rollback"}, args...)
	allArgs = append(allArgs, "--state-root", stateRoot, "--platform", platform, "--dry-run")
	root.SetArgs(allArgs)
	err := root.Execute()
	return buf.String(), err
}

// writeRollbackPlatform creates a platform with two modules ("current" and
// "frozen") for the same resource type. The rule picks "current"; the test
// seeds a past deployment where the resource was applied with "frozen".
// A rollback should re-apply with "frozen" — never "current".
func writeRollbackPlatform(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	platform := `
apiVersion: openporch/v1alpha1
kind: ResourceType
id: workload
---
apiVersion: openporch/v1alpha1
kind: Module
id: current
resource_type: workload
module_source: inline
module_source_code: |
  output "ok" { value = "current" }
---
apiVersion: openporch/v1alpha1
kind: Module
id: frozen
resource_type: workload
module_source: inline
module_source_code: |
  output "ok" { value = "frozen" }
---
apiVersion: openporch/v1alpha1
kind: ModuleRule
id: catchall
resource_type: workload
module_id: current
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

const rollbackManifest = `apiVersion: openporch/v1alpha1
kind: Application
metadata:
  name: test-app
  project: myproj
workloads:
  api:
    type: workload
`

// seedRollbackTarget writes a finished deployment whose single resource was
// applied with the "frozen" module ID — distinct from what rule matching
// would currently pick.
func seedRollbackTarget(t *testing.T, stateRoot, project, env, id string) {
	t.Helper()
	d, err := db.Open(stateRoot)
	if err != nil {
		t.Fatalf("db.Open: %v", err)
	}
	defer d.Close()
	rec := db.NewRecorder(d)
	ctx := context.Background()
	started := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
	if err := rec.StartDeployment(ctx, deploy.DeploymentRecord{
		ID: id, Project: project, Env: env, EnvType: "local", Mode: "deploy",
		StartedAt: started, ManifestYAML: rollbackManifest, GraphJSON: `{}`,
	}); err != nil {
		t.Fatalf("StartDeployment: %v", err)
	}
	if err := rec.RecordResource(ctx, id, deploy.ResourceRecord{
		ResourceKey: "workload|default|api", Type: "workload", Class: "default",
		ID: "api", ModuleID: "frozen", RunnerID: "local-tofu",
		Status: "applied", LogPath: "/tmp/api.log",
	}); err != nil {
		t.Fatalf("RecordResource: %v", err)
	}
	if err := rec.FinishDeployment(ctx, id, "succeeded", started.Add(time.Minute)); err != nil {
		t.Fatalf("FinishDeployment: %v", err)
	}
}

func TestRollback_PinsToTargetModuleID(t *testing.T) {
	t.Parallel()
	platform := writeRollbackPlatform(t)
	stateRoot := t.TempDir()
	seedRollbackTarget(t, stateRoot, "myproj", "dev", "dep-frozen")

	out, err := runRollback(t, stateRoot, platform, "myproj", "dev", "dep-frozen")
	if err != nil {
		t.Fatalf("runRollback: %v\n%s", err, out)
	}
	if !strings.Contains(out, "Dry-run rollback") {
		t.Errorf("expected dry-run rollback header, got:\n%s", out)
	}
	if !strings.Contains(out, "module=frozen") {
		t.Errorf("expected module=frozen (pin), got:\n%s", out)
	}
	if strings.Contains(out, "module=current") {
		t.Errorf("rule-matched module 'current' leaked through despite override; got:\n%s", out)
	}
}

func TestRollback_DeploymentNotFound(t *testing.T) {
	t.Parallel()
	platform := writeRollbackPlatform(t)
	stateRoot := t.TempDir()
	_, err := runRollback(t, stateRoot, platform, "myproj", "dev", "missing")
	if err == nil {
		t.Fatal("expected error, got nil")
	}
	if !strings.Contains(err.Error(), `deployment "missing" not found`) {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestRollback_ProjectMismatch(t *testing.T) {
	t.Parallel()
	platform := writeRollbackPlatform(t)
	stateRoot := t.TempDir()
	seedRollbackTarget(t, stateRoot, "myproj", "dev", "dep-frozen")

	_, err := runRollback(t, stateRoot, platform, "otherproj", "dev", "dep-frozen")
	if err == nil {
		t.Fatal("expected project-mismatch error, got nil")
	}
	if !strings.Contains(err.Error(), `belongs to project "myproj"`) {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestRollback_EnvMismatch(t *testing.T) {
	t.Parallel()
	platform := writeRollbackPlatform(t)
	stateRoot := t.TempDir()
	seedRollbackTarget(t, stateRoot, "myproj", "dev", "dep-frozen")

	_, err := runRollback(t, stateRoot, platform, "myproj", "prod", "dep-frozen")
	if err == nil {
		t.Fatal("expected env-mismatch error, got nil")
	}
	if !strings.Contains(err.Error(), `belongs to env "dev"`) {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestRollback_DryRunAndPlanOnlyExclusive(t *testing.T) {
	t.Parallel()
	platform := writeRollbackPlatform(t)
	stateRoot := t.TempDir()
	seedRollbackTarget(t, stateRoot, "myproj", "dev", "dep-frozen")

	root := &cobra.Command{Use: "openporch", SilenceUsage: true, SilenceErrors: true}
	root.AddCommand(newRollbackCmd())
	var buf bytes.Buffer
	root.SetOut(&buf)
	root.SetErr(&buf)
	root.SetArgs([]string{"rollback", "myproj", "dev", "dep-frozen",
		"--state-root", stateRoot, "--platform", platform,
		"--dry-run", "--plan-only"})
	err := root.Execute()
	if err == nil {
		t.Fatal("expected error, got nil")
	}
	if !strings.Contains(err.Error(), "mutually exclusive") {
		t.Fatalf("unexpected error: %v", err)
	}
}
