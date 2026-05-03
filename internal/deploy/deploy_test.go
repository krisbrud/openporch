package deploy

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"testing"
	"time"

	v1 "github.com/krbrudeli/openporch/api/v1alpha1"
	"github.com/krbrudeli/openporch/internal/runner"
	"github.com/krbrudeli/openporch/internal/store"
)

// stubRunner satisfies runner.Runner without invoking tofu.
type stubRunner struct {
	outputs  map[string]any
	applyCnt int
	planCnt  int
	planPath string
}

func (s *stubRunner) Apply(_ context.Context, _, _ string) (*runner.Result, error) {
	s.applyCnt++
	return &runner.Result{Outputs: s.outputs}, nil
}

func (s *stubRunner) Plan(_ context.Context, workdir, _ string) (string, error) {
	s.planCnt++
	if s.planPath != "" {
		return s.planPath, nil
	}
	return filepath.Join(workdir, "tfplan.bin"), nil
}

func (s *stubRunner) Destroy(_ context.Context, _, _ string) error {
	return nil
}

// captureRecorder captures every ResourceRecord written by the pipeline.
type captureRecorder struct {
	resources []ResourceRecord
}

func (c *captureRecorder) StartDeployment(_ context.Context, _ DeploymentRecord) error { return nil }
func (c *captureRecorder) RecordResource(_ context.Context, _ string, r ResourceRecord) error {
	c.resources = append(c.resources, r)
	return nil
}
func (c *captureRecorder) FinishDeployment(_ context.Context, _ string, _ string, _ time.Time) error {
	return nil
}

// minimalPlatform returns a PlatformConfig with one resource type and one
// inline module wired up by a catch-all rule. No providers are needed.
func minimalPlatform(t *testing.T) *v1.PlatformConfig {
	t.Helper()
	rt := v1.ResourceType{ID: "workload"}
	mod := v1.Module{
		ID:               "workload-stub",
		ResourceType:     "workload",
		ModuleSource:     "inline",
		ModuleSourceCode: ``,
	}
	rule := v1.ModuleRule{
		ID: "catchall", ResourceType: "workload", ModuleID: "workload-stub",
	}
	return &v1.PlatformConfig{
		RootDir:       t.TempDir(),
		ResourceTypes: map[string]v1.ResourceType{"workload": rt},
		Modules:       map[string]v1.Module{"workload-stub": mod},
		ModuleRules:   []v1.ModuleRule{rule},
		Providers:     map[string]v1.Provider{},
		Runners:       map[string]v1.Runner{},
	}
}

// minimalManifest returns a one-workload manifest that exercises the pipeline.
func minimalManifest() *v1.Manifest {
	return &v1.Manifest{
		APIVersion: v1.APIVersion,
		Kind:       v1.KindApplication,
		Metadata:   v1.ManifestMetadata{Name: "test-app"},
		Workloads: map[string]v1.Workload{
			"api": {Type: "workload"},
		},
	}
}

func TestRun_runnerIDPropagatedToRecorder(t *testing.T) {
	rec := &captureRecorder{}
	_, err := Run(context.Background(), Options{
		Manifest:  minimalManifest(),
		Platform:  minimalPlatform(t),
		Store:     &store.FS{Root: t.TempDir()},
		Runner:    &stubRunner{},
		RunnerID:  "my-configured-runner",
		ProjectID: "proj",
		EnvID:     "test",
		EnvTypeID: "local",
		Recorder:  rec,
	})
	if err != nil {
		t.Fatalf("Run: %v", err)
	}
	if len(rec.resources) == 0 {
		t.Fatal("no resources were recorded")
	}
	for _, r := range rec.resources {
		if r.RunnerID != "my-configured-runner" {
			t.Errorf("resource %q: RunnerID = %q, want my-configured-runner", r.ResourceKey, r.RunnerID)
		}
	}
}

func TestRun_runnerIDFallsBackToTypeDerivedStringWhenUnset(t *testing.T) {
	stub := &stubRunner{}
	rec := &captureRecorder{}
	_, err := Run(context.Background(), Options{
		Manifest: minimalManifest(),
		Platform: minimalPlatform(t),
		Store:    &store.FS{Root: t.TempDir()},
		Runner:   stub,
		// RunnerID intentionally not set; pipeline falls back to runnerID(o.Runner).
		ProjectID: "proj",
		EnvID:     "test",
		EnvTypeID: "local",
		Recorder:  rec,
	})
	if err != nil {
		t.Fatalf("Run: %v", err)
	}
	if len(rec.resources) == 0 {
		t.Fatal("no resources were recorded")
	}
	wantID := fmt.Sprintf("%T", stub)
	for _, r := range rec.resources {
		if r.RunnerID != wantID {
			t.Errorf("resource %q: RunnerID = %q, want %q (type-derived fallback)", r.ResourceKey, r.RunnerID, wantID)
		}
	}
}

func TestRun_DryRunSkipsRunner(t *testing.T) {
	t.Parallel()
	stub := &stubRunner{}
	rec := &captureRecorder{}
	stateRoot := t.TempDir()

	res, err := Run(context.Background(), Options{
		Manifest:  minimalManifest(),
		Platform:  minimalPlatform(t),
		Store:     &store.FS{Root: stateRoot},
		Runner:    stub,
		RunnerID:  "local-tofu",
		ProjectID: "proj",
		EnvID:     "test",
		EnvTypeID: "local",
		Recorder:  rec,
		DryRun:    true,
	})
	if err != nil {
		t.Fatalf("Run: %v", err)
	}
	if stub.applyCnt != 0 {
		t.Errorf("Runner.Apply called %d time(s); want 0 in dry-run", stub.applyCnt)
	}
	if len(rec.resources) != 0 {
		t.Errorf("recorder captured %d resource(s); want 0 in dry-run", len(rec.resources))
	}
	if len(res.DryRunResources) != 1 {
		t.Fatalf("DryRunResources len = %d, want 1", len(res.DryRunResources))
	}
	got := res.DryRunResources[0]
	if got.ModuleID != "workload-stub" {
		t.Errorf("DryRunResources[0].ModuleID = %q, want workload-stub", got.ModuleID)
	}
}

func TestRun_DryRunNoStateFiles(t *testing.T) {
	t.Parallel()
	stateRoot := t.TempDir()

	_, err := Run(context.Background(), Options{
		Manifest:  minimalManifest(),
		Platform:  minimalPlatform(t),
		Store:     &store.FS{Root: stateRoot},
		Runner:    &stubRunner{},
		ProjectID: "proj",
		EnvID:     "test",
		EnvTypeID: "local",
		DryRun:    true,
	})
	if err != nil {
		t.Fatalf("Run: %v", err)
	}
	stateDir := filepath.Join(stateRoot, "state")
	if _, err := os.Stat(stateDir); err == nil {
		t.Errorf("dry-run created state directory %s; want none", stateDir)
	}
}

// captureFinish captures every FinishDeployment status to assert on plan-only finals.
type captureFinish struct {
	captureRecorder
	finishStatus string
}

func (c *captureFinish) FinishDeployment(_ context.Context, _ string, status string, _ time.Time) error {
	c.finishStatus = status
	return nil
}

func TestRun_PlanOnlyCallsPlanNotApply(t *testing.T) {
	t.Parallel()
	stub := &stubRunner{}
	rec := &captureFinish{}
	stateRoot := t.TempDir()

	_, err := Run(context.Background(), Options{
		Manifest:  minimalManifest(),
		Platform:  minimalPlatform(t),
		Store:     &store.FS{Root: stateRoot},
		Runner:    stub,
		RunnerID:  "local-tofu",
		ProjectID: "proj",
		EnvID:     "test",
		EnvTypeID: "local",
		Recorder:  rec,
		PlanOnly:  true,
	})
	if err != nil {
		t.Fatalf("Run: %v", err)
	}
	if stub.applyCnt != 0 {
		t.Errorf("Runner.Apply called %d time(s); want 0 in plan-only", stub.applyCnt)
	}
	if stub.planCnt != 1 {
		t.Errorf("Runner.Plan called %d time(s); want 1", stub.planCnt)
	}
	if rec.finishStatus != "planned" {
		t.Errorf("FinishDeployment status = %q, want planned", rec.finishStatus)
	}
	// Last record per resource must be status=planned with non-empty PlanPath.
	last := map[string]ResourceRecord{}
	for _, r := range rec.resources {
		last[r.ResourceKey] = r
	}
	if len(last) == 0 {
		t.Fatal("no resources recorded")
	}
	for k, r := range last {
		if r.Status != "planned" {
			t.Errorf("resource %q: final Status = %q, want planned", k, r.Status)
		}
		if r.PlanPath == "" {
			t.Errorf("resource %q: PlanPath is empty", k)
		}
	}
}

func TestRun_PlanOnlyRecordsPlanOnlyMode(t *testing.T) {
	t.Parallel()
	stub := &stubRunner{}
	started := false
	rec := &startCapturingRecorder{
		onStart: func(d DeploymentRecord) {
			started = true
			if d.Mode != "plan_only" {
				t.Errorf("StartDeployment Mode = %q, want plan_only", d.Mode)
			}
		},
	}
	if _, err := Run(context.Background(), Options{
		Manifest:  minimalManifest(),
		Platform:  minimalPlatform(t),
		Store:     &store.FS{Root: t.TempDir()},
		Runner:    stub,
		RunnerID:  "local-tofu",
		ProjectID: "proj",
		EnvID:     "test",
		EnvTypeID: "local",
		Recorder:  rec,
		PlanOnly:  true,
	}); err != nil {
		t.Fatalf("Run: %v", err)
	}
	if !started {
		t.Error("StartDeployment was not invoked")
	}
}

// startCapturingRecorder is a Recorder that invokes a callback on StartDeployment.
type startCapturingRecorder struct {
	onStart func(DeploymentRecord)
}

func (s *startCapturingRecorder) StartDeployment(_ context.Context, d DeploymentRecord) error {
	if s.onStart != nil {
		s.onStart(d)
	}
	return nil
}
func (s *startCapturingRecorder) RecordResource(_ context.Context, _ string, _ ResourceRecord) error {
	return nil
}
func (s *startCapturingRecorder) FinishDeployment(_ context.Context, _ string, _ string, _ time.Time) error {
	return nil
}

func TestRun_DryRunWritesRenderDir(t *testing.T) {
	t.Parallel()
	renderDir := t.TempDir()

	_, err := Run(context.Background(), Options{
		Manifest:  minimalManifest(),
		Platform:  minimalPlatform(t),
		Store:     &store.FS{Root: t.TempDir()},
		Runner:    &stubRunner{},
		ProjectID: "proj",
		EnvID:     "test",
		EnvTypeID: "local",
		DryRun:    true,
		RenderDir: renderDir,
	})
	if err != nil {
		t.Fatalf("Run: %v", err)
	}
	// At least one main.tf should have been written under renderDir.
	var found bool
	_ = filepath.WalkDir(renderDir, func(path string, d os.DirEntry, err error) error {
		if err == nil && !d.IsDir() && d.Name() == "main.tf" {
			found = true
		}
		return nil
	})
	if !found {
		t.Errorf("no main.tf found under render-dir %s", renderDir)
	}
}
