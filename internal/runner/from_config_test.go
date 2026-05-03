package runner

import (
	"strings"
	"testing"

	v1 "github.com/krbrudeli/openporch/api/v1alpha1"
)

func TestFromConfig_localTofu(t *testing.T) {
	r, err := FromConfig(v1.Runner{ID: "lt", Type: v1.RunnerLocalTofu}, "/usr/bin/tofu", "/tmp/cache")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	lt, ok := r.(*LocalTofu)
	if !ok {
		t.Fatalf("expected *LocalTofu, got %T", r)
	}
	if lt.BinaryPath != "/usr/bin/tofu" {
		t.Errorf("BinaryPath = %q, want /usr/bin/tofu", lt.BinaryPath)
	}
	if lt.PluginCacheDir != "/tmp/cache" {
		t.Errorf("PluginCacheDir = %q, want /tmp/cache", lt.PluginCacheDir)
	}
}

func TestFromConfig_emptyBinaryPath(t *testing.T) {
	r, err := FromConfig(v1.Runner{ID: "lt", Type: v1.RunnerLocalTofu}, "", "")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	lt := r.(*LocalTofu)
	if lt.BinaryPath != "" {
		t.Errorf("BinaryPath = %q, want empty (PATH lookup)", lt.BinaryPath)
	}
}

func TestFromConfig_unsupportedType(t *testing.T) {
	_, err := FromConfig(v1.Runner{ID: "cloud-runner", Type: "eks-fargate"}, "", "")
	if err == nil {
		t.Fatal("expected error for unsupported type, got nil")
	}
	msg := err.Error()
	if !strings.Contains(msg, "eks-fargate") {
		t.Errorf("error %q does not mention type", msg)
	}
	if !strings.Contains(msg, "cloud-runner") {
		t.Errorf("error %q does not mention runner ID", msg)
	}
}
