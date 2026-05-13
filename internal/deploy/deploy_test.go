package deploy

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/krbrudeli/openporch/internal/graph"

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
func (c *captureRecorder) SetActiveResources(_ context.Context, _, _, _ string, _ []ActiveResourceRecord) error {
	return nil
}
func (c *captureRecorder) ClearActiveResources(_ context.Context, _, _ string) error { return nil }

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
func (s *startCapturingRecorder) SetActiveResources(_ context.Context, _, _, _ string, _ []ActiveResourceRecord) error {
	return nil
}
func (s *startCapturingRecorder) ClearActiveResources(_ context.Context, _, _ string) error {
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
func (r *recorderFunc) SetActiveResources(_ context.Context, _, _, _ string, _ []ActiveResourceRecord) error {
	return nil
}
func (r *recorderFunc) ClearActiveResources(_ context.Context, _, _ string) error { return nil }

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

// TestRenderGraph_Basic verifies that RenderGraph returns one RenderedResource
// per node, in topological order, with the correct resource keys.
func TestRenderGraph_Basic(t *testing.T) {
	t.Parallel()
	plat := twoTypePlatform(t)

	g := graph.New()
	dbNode := &graph.Node{
		Key: "database|default|shared.db", Type: "database", Class: "default",
		ID: "shared.db", ModuleID: "database-stub",
		Aliases: []string{"shared.db"}, Params: map[string]any{},
	}
	wlNode := &graph.Node{
		Key: "workload|default|api", Type: "workload", Class: "default",
		ID: "api", ModuleID: "workload-stub",
		Aliases: []string{"workloads.api"}, Edges: []string{"database|default|shared.db"},
		Params: map[string]any{},
	}
	if err := g.AddOrMerge(dbNode); err != nil {
		t.Fatalf("AddOrMerge db: %v", err)
	}
	if err := g.AddOrMerge(wlNode); err != nil {
		t.Fatalf("AddOrMerge workload: %v", err)
	}

	got, err := RenderGraph(RenderOptions{
		Platform: plat, Graph: g,
		ProjectID: "proj", EnvID: "env", EnvTypeID: "local",
	})
	if err != nil {
		t.Fatalf("RenderGraph: %v", err)
	}

	wantKeys := []string{"database|default|shared.db", "workload|default|api"}
	gotKeys := make([]string, len(got))
	for i, r := range got {
		gotKeys[i] = r.Key
	}
	if diff := cmp.Diff(wantKeys, gotKeys); diff != "" {
		t.Errorf("rendered keys (-want +got):\n%s", diff)
	}
	for _, r := range got {
		if r.HCL == "" {
			t.Errorf("resource %q: rendered HCL is empty", r.Key)
		}
	}
}

// TestRenderGraph_SingleNode verifies that RenderGraph works for a graph with
// a single node.
func TestRenderGraph_SingleNode(t *testing.T) {
	t.Parallel()
	plat := minimalPlatform(t)

	g := graph.New()
	if err := g.AddOrMerge(&graph.Node{
		Key: "workload|default|api", Type: "workload", Class: "default",
		ID: "api", ModuleID: "workload-stub", Params: map[string]any{},
	}); err != nil {
		t.Fatalf("AddOrMerge: %v", err)
	}

	got, err := RenderGraph(RenderOptions{
		Platform: plat, Graph: g,
		ProjectID: "proj", EnvID: "env", EnvTypeID: "local",
	})
	if err != nil {
		t.Fatalf("RenderGraph: %v", err)
	}
	if len(got) != 1 {
		t.Fatalf("RenderGraph returned %d resources, want 1", len(got))
	}
	if diff := cmp.Diff("workload|default|api", got[0].Key); diff != "" {
		t.Errorf("resource key (-want +got):\n%s", diff)
	}
	if got[0].HCL == "" {
		t.Error("rendered HCL is empty")
	}
}

// TestRenderGraph_NilInputs verifies that RenderGraph returns errors for
// missing required inputs.
func TestRenderGraph_NilInputs(t *testing.T) {
	t.Parallel()
	plat := minimalPlatform(t)
	g := graph.New()

	if _, err := RenderGraph(RenderOptions{Platform: nil, Graph: g}); err == nil {
		t.Error("expected error for nil platform, got nil")
	}
	if _, err := RenderGraph(RenderOptions{Platform: plat, Graph: nil}); err == nil {
		t.Error("expected error for nil graph, got nil")
	}
}

// twoModulePlatform has two modules for the same resource type. The rule
// picks "current"; tests use ModuleOverrides to force "frozen" instead so we
// can verify rollback's pin overrides the current rules.
func twoModulePlatform(t *testing.T) *v1.PlatformConfig {
	t.Helper()
	return &v1.PlatformConfig{
		RootDir: t.TempDir(),
		ResourceTypes: map[string]v1.ResourceType{
			"workload": {ID: "workload"},
		},
		Modules: map[string]v1.Module{
			"current": {
				ID: "current", ResourceType: "workload",
				ModuleSource: "inline", ModuleSourceCode: ``,
			},
			"frozen": {
				ID: "frozen", ResourceType: "workload",
				ModuleSource: "inline", ModuleSourceCode: ``,
			},
		},
		ModuleRules: []v1.ModuleRule{
			{ID: "catchall", ResourceType: "workload", ModuleID: "current"},
		},
		Providers: map[string]v1.Provider{},
		Runners:   map[string]v1.Runner{},
	}
}

func TestRun_ModuleOverridesPinModuleID(t *testing.T) {
	t.Parallel()
	capRec := &captureRecorder{}
	_, err := Run(context.Background(), Options{
		Manifest:  minimalManifest(),
		Platform:  twoModulePlatform(t),
		Store:     &storetest.Fake{},
		Runner:    &runnertest.Recording{},
		RunnerID:  "local-tofu",
		ProjectID: "proj",
		EnvID:     "test",
		EnvTypeID: "local",
		Recorder:  capRec,
		ModuleOverrides: map[string]string{
			"workload|default|api": "frozen",
		},
	})
	if err != nil {
		t.Fatalf("Run: %v", err)
	}
	last := map[string]ResourceRecord{}
	for _, r := range capRec.resources {
		last[r.ResourceKey] = r
	}
	got, ok := last["workload|default|api"]
	if !ok {
		t.Fatalf("workload|default|api was not recorded; got keys: %v", keys(last))
	}
	if got.ModuleID != "frozen" {
		t.Errorf("ModuleID = %q, want frozen (override should bypass rules)", got.ModuleID)
	}
}

func TestRun_ModuleOverridesFallBackToRulesForUnpinnedNodes(t *testing.T) {
	t.Parallel()
	capRec := &captureRecorder{}
	// Overrides intentionally empty — pipeline must still use rule matching.
	_, err := Run(context.Background(), Options{
		Manifest:        minimalManifest(),
		Platform:        twoModulePlatform(t),
		Store:           &storetest.Fake{},
		Runner:          &runnertest.Recording{},
		RunnerID:        "local-tofu",
		ProjectID:       "proj",
		EnvID:           "test",
		EnvTypeID:       "local",
		Recorder:        capRec,
		ModuleOverrides: map[string]string{},
	})
	if err != nil {
		t.Fatalf("Run: %v", err)
	}
	last := map[string]ResourceRecord{}
	for _, r := range capRec.resources {
		last[r.ResourceKey] = r
	}
	got := last["workload|default|api"]
	if got.ModuleID != "current" {
		t.Errorf("ModuleID = %q, want current (rules should apply when overrides empty)", got.ModuleID)
	}
}

func TestRun_RollbackModeIsRecorded(t *testing.T) {
	t.Parallel()
	gotMode := ""
	rec := &startCapturingRecorder{
		onStart: func(d DeploymentRecord) { gotMode = d.Mode },
	}
	if _, err := Run(context.Background(), Options{
		Manifest:        minimalManifest(),
		Platform:        twoModulePlatform(t),
		Store:           &storetest.Fake{},
		Runner:          &runnertest.Recording{},
		RunnerID:        "local-tofu",
		ProjectID:       "proj",
		EnvID:           "test",
		EnvTypeID:       "local",
		Recorder:        rec,
		Mode:            "rollback",
		ModuleOverrides: map[string]string{"workload|default|api": "frozen"},
	}); err != nil {
		t.Fatalf("Run: %v", err)
	}
	if gotMode != "rollback" {
		t.Errorf("StartDeployment Mode = %q, want rollback", gotMode)
	}
}

func keys(m map[string]ResourceRecord) []string {
	out := make([]string, 0, len(m))
	for k := range m {
		out = append(out, k)
	}
	return out
}
