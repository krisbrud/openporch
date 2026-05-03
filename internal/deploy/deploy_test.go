package deploy

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/google/go-cmp/cmp"
	v1 "github.com/krbrudeli/openporch/api/v1alpha1"
	"github.com/krbrudeli/openporch/internal/runner/runnertest"
	"github.com/krbrudeli/openporch/internal/store/storetest"
)

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

// captureFinish captures every FinishDeployment status.
type captureFinish struct {
	captureRecorder
	finishStatus string
}

func (c *captureFinish) FinishDeployment(_ context.Context, _ string, status string, _ time.Time) error {
	c.finishStatus = status
	return nil
}

// startCapturingRecorder invokes a callback on StartDeployment.
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

// recorderFunc is a Recorder backed by function fields for precise event capture.
type recorderFunc struct {
	onStart    func(DeploymentRecord) error
	onResource func(ResourceRecord) error
	onFinish   func(status string) error
}

func (r *recorderFunc) StartDeployment(_ context.Context, d DeploymentRecord) error {
	if r.onStart != nil {
		return r.onStart(d)
	}
	return nil
}
func (r *recorderFunc) RecordResource(_ context.Context, _ string, rec ResourceRecord) error {
	if r.onResource != nil {
		return r.onResource(rec)
	}
	return nil
}
func (r *recorderFunc) FinishDeployment(_ context.Context, _ string, status string, _ time.Time) error {
	if r.onFinish != nil {
		return r.onFinish(status)
	}
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

// twoTypePlatform returns a PlatformConfig with "workload" and "database"
// resource types, each backed by an empty inline module.
func twoTypePlatform(t *testing.T) *v1.PlatformConfig {
	t.Helper()
	return &v1.PlatformConfig{
		RootDir: t.TempDir(),
		ResourceTypes: map[string]v1.ResourceType{
			"workload": {ID: "workload"},
			"database": {ID: "database"},
		},
		Modules: map[string]v1.Module{
			"workload-stub": {
				ID: "workload-stub", ResourceType: "workload",
				ModuleSource: "inline", ModuleSourceCode: ``,
			},
			"database-stub": {
				ID: "database-stub", ResourceType: "database",
				ModuleSource: "inline", ModuleSourceCode: ``,
			},
		},
		ModuleRules: []v1.ModuleRule{
			{ID: "workload-rule", ResourceType: "workload", ModuleID: "workload-stub"},
			{ID: "database-rule", ResourceType: "database", ModuleID: "database-stub"},
		},
		Providers: map[string]v1.Provider{},
		Runners:   map[string]v1.Runner{},
	}
}

// dependencyManifest returns a manifest where workload "api" declares a
// workload-scoped resource "db" of type "database". The graph builder adds an
// edge so "db" must be applied before "api".
func dependencyManifest() *v1.Manifest {
	return &v1.Manifest{
		APIVersion: v1.APIVersion,
		Kind:       v1.KindApplication,
		Metadata:   v1.ManifestMetadata{Name: "test-app"},
		Workloads: map[string]v1.Workload{
			"api": {
				Type: "workload",
				Resources: map[string]v1.ResourceRef{
					"db": {Type: "database"},
				},
			},
		},
	}
}

// callsOfOp filters runnertest.Calls by operation name.
func callsOfOp(calls []runnertest.Call, op string) []runnertest.Call {
	var out []runnertest.Call
	for _, c := range calls {
		if c.Op == op {
			out = append(out, c)
		}
	}
	return out
}

func TestRun_runnerIDPropagatedToRecorder(t *testing.T) {
	rec := &captureRecorder{}
	_, err := Run(context.Background(), Options{
		Manifest:  minimalManifest(),
		Platform:  minimalPlatform(t),
		Store:     &storetest.Fake{},
		Runner:    &runnertest.Recording{},
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
	stub := &runnertest.Recording{}
	rec := &captureRecorder{}
	_, err := Run(context.Background(), Options{
		Manifest: minimalManifest(),
		Platform: minimalPlatform(t),
		Store:    &storetest.Fake{},
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
	rec := &runnertest.Recording{}
	capRec := &captureRecorder{}

	res, err := Run(context.Background(), Options{
		Manifest:  minimalManifest(),
		Platform:  minimalPlatform(t),
		Store:     &storetest.Fake{},
		Runner:    rec,
		RunnerID:  "local-tofu",
		ProjectID: "proj",
		EnvID:     "test",
		EnvTypeID: "local",
		Recorder:  capRec,
		DryRun:    true,
	})
	if err != nil {
		t.Fatalf("Run: %v", err)
	}
	if n := len(callsOfOp(rec.Calls, "Apply")); n != 0 {
		t.Errorf("Runner.Apply called %d time(s); want 0 in dry-run", n)
	}
	if len(capRec.resources) != 0 {
		t.Errorf("recorder captured %d resource(s); want 0 in dry-run", len(capRec.resources))
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
	fake := &storetest.Fake{}

	_, err := Run(context.Background(), Options{
		Manifest:  minimalManifest(),
		Platform:  minimalPlatform(t),
		Store:     fake,
		Runner:    &runnertest.Recording{},
		ProjectID: "proj",
		EnvID:     "test",
		EnvTypeID: "local",
		DryRun:    true,
	})
	if err != nil {
		t.Fatalf("Run: %v", err)
	}
	if len(fake.Writes) != 0 {
		t.Errorf("dry-run wrote %d file(s) via store; want 0", len(fake.Writes))
	}
}

func TestRun_PlanOnlyCallsPlanNotApply(t *testing.T) {
	t.Parallel()
	rec := &runnertest.Recording{}
	capRec := &captureFinish{}

	_, err := Run(context.Background(), Options{
		Manifest:  minimalManifest(),
		Platform:  minimalPlatform(t),
		Store:     &storetest.Fake{},
		Runner:    rec,
		RunnerID:  "local-tofu",
		ProjectID: "proj",
		EnvID:     "test",
		EnvTypeID: "local",
		Recorder:  capRec,
		PlanOnly:  true,
	})
	if err != nil {
		t.Fatalf("Run: %v", err)
	}
	if n := len(callsOfOp(rec.Calls, "Apply")); n != 0 {
		t.Errorf("Runner.Apply called %d time(s); want 0 in plan-only", n)
	}
	if n := len(callsOfOp(rec.Calls, "Plan")); n != 1 {
		t.Errorf("Runner.Plan called %d time(s); want 1", n)
	}
	if capRec.finishStatus != "planned" {
		t.Errorf("FinishDeployment status = %q, want planned", capRec.finishStatus)
	}
	last := map[string]ResourceRecord{}
	for _, r := range capRec.resources {
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
		Store:     &storetest.Fake{},
		Runner:    &runnertest.Recording{},
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

func TestRun_DryRunWritesRenderDir(t *testing.T) {
	t.Parallel()
	renderDir := t.TempDir()

	_, err := Run(context.Background(), Options{
		Manifest:  minimalManifest(),
		Platform:  minimalPlatform(t),
		Store:     &storetest.Fake{},
		Runner:    &runnertest.Recording{},
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

// TestRun_GraphOrdering verifies that when a workload declares a database
// resource, the graph orders database before workload (dependency before
// dependent).
func TestRun_GraphOrdering(t *testing.T) {
	t.Parallel()
	fake := &storetest.Fake{}
	rec := &runnertest.Recording{}

	_, err := Run(context.Background(), Options{
		Manifest:  dependencyManifest(),
		Platform:  twoTypePlatform(t),
		Store:     fake,
		Runner:    rec,
		ProjectID: "proj",
		EnvID:     "env",
		EnvTypeID: "local",
	})
	if err != nil {
		t.Fatalf("Run: %v", err)
	}

	applies := callsOfOp(rec.Calls, "Apply")
	if len(applies) != 2 {
		t.Fatalf("Apply called %d time(s); want 2 (one per resource)", len(applies))
	}

	// database|default|workloads.api.db must apply before workload|default|api.
	dbWorkdir := fake.ResourceDir("proj", "env", "database|default|workloads.api.db")
	wlWorkdir := fake.ResourceDir("proj", "env", "workload|default|api")

	if diff := cmp.Diff(dbWorkdir, applies[0].Workdir); diff != "" {
		t.Errorf("first Apply workdir mismatch (-want +got):\n%s\n(database must apply before workload)", diff)
	}
	if diff := cmp.Diff(wlWorkdir, applies[1].Workdir); diff != "" {
		t.Errorf("second Apply workdir mismatch (-want +got):\n%s\n(workload must apply after database)", diff)
	}
}

// TestRun_ErrorPropagation verifies that when Apply fails for a resource,
// Run returns an error and stops processing further resources.
func TestRun_ErrorPropagation(t *testing.T) {
	t.Parallel()
	fake := &storetest.Fake{}
	wlWorkdir := fake.ResourceDir("proj", "env", "workload|default|api")
	rec := &runnertest.Recording{
		ApplyErr: map[string]error{
			wlWorkdir: fmt.Errorf("injected apply failure"),
		},
	}

	_, err := Run(context.Background(), Options{
		Manifest:  minimalManifest(),
		Platform:  minimalPlatform(t),
		Store:     fake,
		Runner:    rec,
		ProjectID: "proj",
		EnvID:     "env",
		EnvTypeID: "local",
	})
	if err == nil {
		t.Fatal("Run returned nil; want error from injected apply failure")
	}

	applies := callsOfOp(rec.Calls, "Apply")
	if len(applies) != 1 {
		t.Errorf("Apply called %d time(s); want 1 (pipeline must stop after first failure)", len(applies))
	}
}

// TestRun_RecorderSideEffects verifies the recorder event sequence:
// StartDeployment → RecordResource(applying) → RecordResource(applied) → FinishDeployment.
func TestRun_RecorderSideEffects(t *testing.T) {
	t.Parallel()

	type event struct {
		Op     string
		Status string
	}
	var events []event

	rec := &recorderFunc{
		onStart: func(d DeploymentRecord) error {
			events = append(events, event{"start", d.Mode})
			return nil
		},
		onResource: func(r ResourceRecord) error {
			events = append(events, event{"resource", r.Status})
			return nil
		},
		onFinish: func(status string) error {
			events = append(events, event{"finish", status})
			return nil
		},
	}

	if _, err := Run(context.Background(), Options{
		Manifest:  minimalManifest(),
		Platform:  minimalPlatform(t),
		Store:     &storetest.Fake{},
		Runner:    &runnertest.Recording{},
		ProjectID: "proj",
		EnvID:     "env",
		EnvTypeID: "local",
		Recorder:  rec,
	}); err != nil {
		t.Fatalf("Run: %v", err)
	}

	want := []event{
		{"start", "deploy"},
		{"resource", "applying"},
		{"resource", "applied"},
		{"finish", "succeeded"},
	}
	if diff := cmp.Diff(want, events); diff != "" {
		t.Errorf("recorder event sequence mismatch (-want +got):\n%s", diff)
	}
}
