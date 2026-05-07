package main

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/google/go-cmp/cmp"

	"github.com/krbrudeli/openporch/internal/deploy"
	"github.com/krbrudeli/openporch/internal/store/db"
)

func TestParseManifestSource(t *testing.T) {
	t.Parallel()
	tests := []struct {
		name    string
		arg     string
		want    manifestSource
		wantErr string
	}{
		{
			name: "file path",
			arg:  "manifest.yaml",
			want: manifestSource{File: "manifest.yaml"},
		},
		{
			name: "file path with directories",
			arg:  "./examples/foo/manifest.yaml",
			want: manifestSource{File: "./examples/foo/manifest.yaml"},
		},
		{
			name: "deployment HEAD",
			arg:  "deployment://HEAD",
			want: manifestSource{Deployment: "HEAD", Head: true},
		},
		{
			name: "deployment by id",
			arg:  "deployment://dep-abc123",
			want: manifestSource{Deployment: "dep-abc123", DeploymentID: "dep-abc123"},
		},
		{
			name: "environment",
			arg:  "environment://staging",
			want: manifestSource{Environment: "staging"},
		},
		{
			name:    "empty",
			arg:     "",
			wantErr: "manifest source is required",
		},
		{
			name:    "deployment without ref",
			arg:     "deployment://",
			wantErr: "deployment source requires HEAD",
		},
		{
			name:    "environment without ref",
			arg:     "environment://",
			wantErr: "environment source requires",
		},
		{
			name:    "unknown scheme",
			arg:     "s3://bucket/manifest.yaml",
			wantErr: `unknown manifest source scheme "s3"`,
		},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			got, err := parseManifestSource(tc.arg)
			if tc.wantErr != "" {
				if err == nil {
					t.Fatalf("expected error containing %q, got nil", tc.wantErr)
				}
				if !strings.Contains(err.Error(), tc.wantErr) {
					t.Fatalf("error %v does not contain %q", err, tc.wantErr)
				}
				return
			}
			if err != nil {
				t.Fatalf("unexpected error: %v", err)
			}
			if diff := cmp.Diff(tc.want, got); diff != "" {
				t.Errorf("parseManifestSource(%q) mismatch (-want +got):\n%s", tc.arg, diff)
			}
		})
	}
}

// seedDeployment writes one deployment to the SQLite store and optionally
// marks it succeeded.
func seedDeployment(t *testing.T, rec *db.SQLiteRecorder, id, project, env string, started time.Time, manifestYAML, status string) {
	t.Helper()
	ctx := context.Background()
	if err := rec.StartDeployment(ctx, deploy.DeploymentRecord{
		ID: id, Project: project, Env: env, EnvType: "local",
		Mode: "deploy", StartedAt: started,
		ManifestYAML: manifestYAML, GraphJSON: `{}`,
	}); err != nil {
		t.Fatalf("StartDeployment %s: %v", id, err)
	}
	if status != "" && status != "running" {
		if err := rec.FinishDeployment(ctx, id, status, started.Add(time.Minute)); err != nil {
			t.Fatalf("FinishDeployment %s: %v", id, err)
		}
	}
}

const validManifest = `apiVersion: openporch/v1alpha1
kind: Application
metadata:
  name: test-app
  project: myproj
workloads:
  api:
    resources:
      web:
        type: service
`

func TestResolveManifestSource_File(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	path := filepath.Join(dir, "manifest.yaml")
	if err := os.WriteFile(path, []byte(validManifest), 0o644); err != nil {
		t.Fatalf("write: %v", err)
	}
	m, err := resolveManifestSource(context.Background(), nil,
		manifestSource{File: path}, "", "")
	if err != nil {
		t.Fatalf("resolve: %v", err)
	}
	if m.Metadata.Name != "test-app" {
		t.Errorf("Name = %q, want test-app", m.Metadata.Name)
	}
}

func TestResolveManifestSource_DeploymentHEAD(t *testing.T) {
	t.Parallel()
	d, err := db.Open(t.TempDir())
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	t.Cleanup(func() { d.Close() })
	rec := db.NewRecorder(d)

	base := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
	// Two succeeded deployments and one running — HEAD should return the
	// latest succeeded by started_at.
	seedDeployment(t, rec, "old", "myproj", "dev", base, "old: 1\n"+validManifest, "succeeded")
	seedDeployment(t, rec, "newest", "myproj", "dev", base.Add(2*time.Hour), validManifest, "succeeded")
	seedDeployment(t, rec, "running", "myproj", "dev", base.Add(3*time.Hour), validManifest, "running")
	// Different env should not match.
	seedDeployment(t, rec, "prod-only", "myproj", "prod", base.Add(4*time.Hour), validManifest, "succeeded")

	m, err := resolveManifestSource(context.Background(), db.NewReader(d),
		manifestSource{Deployment: "HEAD", Head: true}, "myproj", "dev")
	if err != nil {
		t.Fatalf("resolve: %v", err)
	}
	if m.Metadata.Name != "test-app" {
		t.Errorf("Name = %q, want test-app", m.Metadata.Name)
	}
}

func TestResolveManifestSource_DeploymentByID(t *testing.T) {
	t.Parallel()
	d, err := db.Open(t.TempDir())
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	t.Cleanup(func() { d.Close() })
	rec := db.NewRecorder(d)

	seedDeployment(t, rec, "dep-target", "p", "e",
		time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC), validManifest, "succeeded")

	m, err := resolveManifestSource(context.Background(), db.NewReader(d),
		manifestSource{Deployment: "dep-target", DeploymentID: "dep-target"}, "", "")
	if err != nil {
		t.Fatalf("resolve: %v", err)
	}
	if m.Metadata.Name != "test-app" {
		t.Errorf("Name = %q, want test-app", m.Metadata.Name)
	}
}

func TestResolveManifestSource_Environment(t *testing.T) {
	t.Parallel()
	d, err := db.Open(t.TempDir())
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	t.Cleanup(func() { d.Close() })
	rec := db.NewRecorder(d)

	base := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
	seedDeployment(t, rec, "stg-1", "myproj", "staging", base, validManifest, "succeeded")
	seedDeployment(t, rec, "stg-2", "myproj", "staging", base.Add(time.Hour),
		strings.Replace(validManifest, "test-app", "from-staging", 1), "succeeded")
	// Wrong project should be ignored.
	seedDeployment(t, rec, "other-stg", "other", "staging", base.Add(2*time.Hour), validManifest, "succeeded")

	m, err := resolveManifestSource(context.Background(), db.NewReader(d),
		manifestSource{Environment: "staging"}, "myproj", "prod")
	if err != nil {
		t.Fatalf("resolve: %v", err)
	}
	if m.Metadata.Name != "from-staging" {
		t.Errorf("Name = %q, want from-staging (latest staging deployment)", m.Metadata.Name)
	}
}

func TestResolveManifestSource_Errors(t *testing.T) {
	t.Parallel()
	d, err := db.Open(t.TempDir())
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	t.Cleanup(func() { d.Close() })
	rec := db.NewRecorder(d)
	// One running (not succeeded) deployment to confirm HEAD ignores it.
	seedDeployment(t, rec, "live", "p", "e",
		time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC), validManifest, "running")
	// One succeeded deployment with empty manifest_yaml to test that branch.
	if err := rec.StartDeployment(context.Background(), deploy.DeploymentRecord{
		ID: "empty", Project: "q", Env: "e", EnvType: "local",
		Mode: "deploy", StartedAt: time.Date(2026, 2, 1, 0, 0, 0, 0, time.UTC),
		ManifestYAML: "", GraphJSON: `{}`,
	}); err != nil {
		t.Fatalf("seed empty: %v", err)
	}
	if err := rec.FinishDeployment(context.Background(), "empty", "succeeded",
		time.Date(2026, 2, 1, 0, 1, 0, 0, time.UTC)); err != nil {
		t.Fatalf("finish empty: %v", err)
	}
	// One succeeded with broken manifest yaml so parseStoredManifest fails.
	if err := rec.StartDeployment(context.Background(), deploy.DeploymentRecord{
		ID: "broken", Project: "r", Env: "e", EnvType: "local",
		Mode: "deploy", StartedAt: time.Date(2026, 3, 1, 0, 0, 0, 0, time.UTC),
		ManifestYAML: "this is: not: valid: yaml:\n", GraphJSON: `{}`,
	}); err != nil {
		t.Fatalf("seed broken: %v", err)
	}
	if err := rec.FinishDeployment(context.Background(), "broken", "succeeded",
		time.Date(2026, 3, 1, 0, 1, 0, 0, time.UTC)); err != nil {
		t.Fatalf("finish broken: %v", err)
	}
	rdr := db.NewReader(d)

	tests := []struct {
		name    string
		src     manifestSource
		project string
		env     string
		wantErr string
	}{
		{
			name:    "HEAD without project",
			src:     manifestSource{Head: true},
			env:     "e",
			wantErr: "deployment://HEAD requires --project and --env",
		},
		{
			name:    "HEAD without env",
			src:     manifestSource{Head: true},
			project: "p",
			wantErr: "deployment://HEAD requires --project and --env",
		},
		{
			name:    "HEAD no successful deployment",
			src:     manifestSource{Head: true},
			project: "p",
			env:     "e",
			wantErr: `no successful deployment found for project="p" env="e"`,
		},
		{
			name:    "deployment id not found",
			src:     manifestSource{DeploymentID: "missing"},
			wantErr: `deployment "missing" not found`,
		},
		{
			name:    "environment without project",
			src:     manifestSource{Environment: "staging"},
			wantErr: "environment://staging requires --project",
		},
		{
			name:    "environment no successful deployment",
			src:     manifestSource{Environment: "ghost"},
			project: "p",
			wantErr: `no successful deployment found for project="p" env="ghost"`,
		},
		{
			name:    "deployment with empty manifest",
			src:     manifestSource{DeploymentID: "empty"},
			wantErr: `deployment "empty" has no stored manifest`,
		},
		{
			name:    "deployment with broken manifest",
			src:     manifestSource{DeploymentID: "broken"},
			wantErr: `deployment "broken"`,
		},
		{
			name:    "no source set",
			src:     manifestSource{},
			wantErr: "manifest source is required",
		},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			_, err := resolveManifestSource(context.Background(), rdr, tc.src, tc.project, tc.env)
			if err == nil {
				t.Fatalf("expected error containing %q, got nil", tc.wantErr)
			}
			if !strings.Contains(err.Error(), tc.wantErr) {
				t.Fatalf("error %v does not contain %q", err, tc.wantErr)
			}
		})
	}
}
