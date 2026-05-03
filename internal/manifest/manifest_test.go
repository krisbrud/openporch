package manifest

import (
	"os"
	"path/filepath"
	"testing"

	"github.com/google/go-cmp/cmp"
	v1 "github.com/krbrudeli/openporch/api/v1alpha1"
)

func writeTemp(t *testing.T, content string) string {
	t.Helper()
	dir := t.TempDir()
	p := filepath.Join(dir, "manifest.yaml")
	if err := os.WriteFile(p, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
	return p
}

func TestLoad_validManifest(t *testing.T) {
	t.Parallel()
	p := writeTemp(t, `
apiVersion: openporch/v1alpha1
kind: Application
metadata:
  name: my-app
  project: demo
workloads:
  api:
    type: workload
    params:
      image: hello:latest
    resources:
      db:
        type: postgres
shared:
  bucket:
    type: s3-bucket
`)
	m, err := Load(p)
	if err != nil {
		t.Fatal(err)
	}
	want := &v1.Manifest{
		APIVersion: v1.APIVersion,
		Kind:       v1.KindApplication,
		Metadata: v1.ManifestMetadata{
			Name:    "my-app",
			Project: "demo",
		},
		Workloads: map[string]v1.Workload{
			"api": {
				Type: "workload",
				Params: map[string]any{
					"image": "hello:latest",
				},
				Resources: map[string]v1.ResourceRef{
					"db": {Type: "postgres"},
				},
			},
		},
		Shared: map[string]v1.ResourceRef{
			"bucket": {Type: "s3-bucket"},
		},
	}
	if diff := cmp.Diff(want, m); diff != "" {
		t.Errorf("Load() mismatch (-want +got):\n%s", diff)
	}
}

func TestLoad_rejectsBadAPIVersion(t *testing.T) {
	t.Parallel()
	p := writeTemp(t, `
apiVersion: bogus
kind: Application
metadata:
  name: x
`)
	if _, err := Load(p); err == nil {
		t.Fatal("expected error")
	}
}

func TestLoad_requiresResourceType(t *testing.T) {
	t.Parallel()
	p := writeTemp(t, `
apiVersion: openporch/v1alpha1
kind: Application
metadata:
  name: x
workloads:
  a:
    resources:
      r: {}
`)
	if _, err := Load(p); err == nil {
		t.Fatal("expected error")
	}
}
