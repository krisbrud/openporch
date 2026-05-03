package deploy

import (
	"context"
	"fmt"
	"testing"
	"time"

	v1 "github.com/krbrudeli/openporch/api/v1alpha1"
	"github.com/krbrudeli/openporch/internal/runner"
	"github.com/krbrudeli/openporch/internal/store"
)

// stubRunner satisfies runner.Runner without invoking tofu.
type stubRunner struct {
	outputs map[string]any
}

func (s *stubRunner) Apply(_ context.Context, _, _ string) (*runner.Result, error) {
	return &runner.Result{Outputs: s.outputs}, nil
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
		Manifest:  minimalManifest(),
		Platform:  minimalPlatform(t),
		Store:     &store.FS{Root: t.TempDir()},
		Runner:    stub,
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
