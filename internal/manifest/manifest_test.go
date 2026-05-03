package manifest

import (
	"os"
	"path/filepath"
	"testing"
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
	if m.Metadata.Name != "my-app" || m.Workloads["api"].Resources["db"].Type != "postgres" {
		t.Errorf("got %+v", m)
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
