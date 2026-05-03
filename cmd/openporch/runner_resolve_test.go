package main

import (
	"path/filepath"
	"strings"
	"testing"

	v1 "github.com/krbrudeli/openporch/api/v1alpha1"
	"github.com/krbrudeli/openporch/internal/runner"
)

func cfg(runners map[string]v1.Runner, rules []v1.RunnerRule) *v1.PlatformConfig {
	return &v1.PlatformConfig{Runners: runners, RunnerRules: rules}
}

func TestResolveRunner_catchAll(t *testing.T) {
	c := cfg(
		map[string]v1.Runner{"lt": {ID: "lt", Type: "local-tofu"}},
		[]v1.RunnerRule{{ID: "catchall", RunnerID: "lt"}},
	)
	id, r, err := resolveRunner(c, "proj", "default", "local", t.TempDir(), "")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if id != "lt" {
		t.Errorf("runner_id = %q, want lt", id)
	}
	if _, ok := r.(*runner.LocalTofu); !ok {
		t.Errorf("expected *runner.LocalTofu, got %T", r)
	}
}

func TestResolveRunner_specificRuleBeatsGeneric(t *testing.T) {
	c := cfg(
		map[string]v1.Runner{
			"generic":  {ID: "generic", Type: "local-tofu"},
			"specific": {ID: "specific", Type: "local-tofu"},
		},
		[]v1.RunnerRule{
			{ID: "catchall", RunnerID: "generic"},
			{ID: "prod", RunnerID: "specific", EnvTypeID: "production"},
		},
	)
	id, _, err := resolveRunner(c, "proj", "default", "production", t.TempDir(), "")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if id != "specific" {
		t.Errorf("runner_id = %q, want specific", id)
	}
}

func TestResolveRunner_pluginCacheUnderStateRoot(t *testing.T) {
	stateRoot := t.TempDir()
	c := cfg(
		map[string]v1.Runner{"lt": {ID: "lt", Type: "local-tofu"}},
		[]v1.RunnerRule{{ID: "catchall", RunnerID: "lt"}},
	)
	_, r, err := resolveRunner(c, "proj", "default", "local", stateRoot, "")
	if err != nil {
		t.Fatalf("unexpected: %v", err)
	}
	lt := r.(*runner.LocalTofu)
	want := filepath.Join(stateRoot, "plugin-cache")
	if lt.PluginCacheDir != want {
		t.Errorf("PluginCacheDir = %q, want %q", lt.PluginCacheDir, want)
	}
}

func TestResolveRunner_binaryPathForwarded(t *testing.T) {
	c := cfg(
		map[string]v1.Runner{"lt": {ID: "lt", Type: "local-tofu"}},
		[]v1.RunnerRule{{ID: "catchall", RunnerID: "lt"}},
	)
	_, r, err := resolveRunner(c, "proj", "default", "local", t.TempDir(), "/custom/tofu")
	if err != nil {
		t.Fatalf("unexpected: %v", err)
	}
	lt := r.(*runner.LocalTofu)
	if lt.BinaryPath != "/custom/tofu" {
		t.Errorf("BinaryPath = %q, want /custom/tofu", lt.BinaryPath)
	}
}

func TestResolveRunner_noMatchNamesContext(t *testing.T) {
	c := cfg(
		map[string]v1.Runner{"cloud": {ID: "cloud", Type: "local-tofu"}},
		[]v1.RunnerRule{{ID: "prod-only", RunnerID: "cloud", EnvTypeID: "production"}},
	)
	_, _, err := resolveRunner(c, "myproj", "myenv", "local", t.TempDir(), "")
	if err == nil {
		t.Fatal("expected error when no rule matches, got nil")
	}
	msg := err.Error()
	for _, want := range []string{"myproj", "myenv", "local"} {
		if !strings.Contains(msg, want) {
			t.Errorf("error %q does not mention %q", msg, want)
		}
	}
}

func TestResolveRunner_unsupportedTypeReturnsError(t *testing.T) {
	c := cfg(
		map[string]v1.Runner{"cloud": {ID: "cloud", Type: "eks-fargate"}},
		[]v1.RunnerRule{{ID: "catchall", RunnerID: "cloud"}},
	)
	_, _, err := resolveRunner(c, "proj", "default", "local", t.TempDir(), "")
	if err == nil {
		t.Fatal("expected error for unsupported type, got nil")
	}
	if !strings.Contains(err.Error(), "eks-fargate") {
		t.Errorf("error %q should mention unsupported type", err.Error())
	}
}
