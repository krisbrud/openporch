package db

import (
	"context"
	"testing"
	"time"

	"github.com/krbrudeli/openporch/internal/deploy"
)

// mustSeed inserts one deployment (and optionally marks it finished) plus any
// supplied resources. All fatal errors are surfaced via t.Fatalf.
func mustSeed(t *testing.T, rec *SQLiteRecorder, d deploy.DeploymentRecord, finished bool, resources []deploy.ResourceRecord) {
	t.Helper()
	ctx := context.Background()
	if err := rec.StartDeployment(ctx, d); err != nil {
		t.Fatalf("StartDeployment %s: %v", d.ID, err)
	}
	for _, r := range resources {
		if err := rec.RecordResource(ctx, d.ID, r); err != nil {
			t.Fatalf("RecordResource %s/%s: %v", d.ID, r.ResourceKey, err)
		}
	}
	if finished {
		if err := rec.FinishDeployment(ctx, d.ID, "succeeded", d.StartedAt.Add(time.Minute)); err != nil {
			t.Fatalf("FinishDeployment %s: %v", d.ID, err)
		}
	}
}

func openTestDB(t *testing.T) (*DB, *SQLiteRecorder, *Reader) {
	t.Helper()
	d, err := Open(t.TempDir())
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	t.Cleanup(func() { d.Close() })
	rec := NewRecorder(d)
	rdr := NewReader(d)
	return d, rec, rdr
}

// ---------------------------------------------------------------------------
// ListDeployments
// ---------------------------------------------------------------------------

func TestListDeployments_Empty(t *testing.T) {
	_, _, rdr := openTestDB(t)
	rows, err := rdr.ListDeployments(context.Background(), "", "", 20)
	if err != nil {
		t.Fatalf("ListDeployments: %v", err)
	}
	if len(rows) != 0 {
		t.Errorf("expected empty slice, got %d rows", len(rows))
	}
}

func TestListDeployments_Order(t *testing.T) {
	_, rec, rdr := openTestDB(t)
	base := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)

	for i, id := range []string{"dep-a", "dep-b", "dep-c"} {
		mustSeed(t, rec, deploy.DeploymentRecord{
			ID: id, Project: "p", Env: "e", EnvType: "local", Mode: "deploy",
			StartedAt:    base.Add(time.Duration(i) * time.Hour),
			ManifestYAML: "kind: Manifest\n", GraphJSON: `{}`,
		}, false, nil)
	}

	rows, err := rdr.ListDeployments(context.Background(), "", "", 20)
	if err != nil {
		t.Fatalf("ListDeployments: %v", err)
	}
	if len(rows) != 3 {
		t.Fatalf("got %d rows, want 3", len(rows))
	}
	// started_at DESC means dep-c (latest) first
	wantOrder := []string{"dep-c", "dep-b", "dep-a"}
	for i, want := range wantOrder {
		if rows[i].ID != want {
			t.Errorf("rows[%d].ID = %q, want %q", i, rows[i].ID, want)
		}
	}
}

func TestListDeployments_ProjectFilter(t *testing.T) {
	_, rec, rdr := openTestDB(t)
	base := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)

	mustSeed(t, rec, deploy.DeploymentRecord{
		ID: "d1", Project: "alpha", Env: "dev", EnvType: "local", Mode: "deploy",
		StartedAt: base, ManifestYAML: "kind: Manifest\n", GraphJSON: `{}`,
	}, false, nil)
	mustSeed(t, rec, deploy.DeploymentRecord{
		ID: "d2", Project: "beta", Env: "dev", EnvType: "local", Mode: "deploy",
		StartedAt: base.Add(time.Minute), ManifestYAML: "kind: Manifest\n", GraphJSON: `{}`,
	}, false, nil)

	rows, err := rdr.ListDeployments(context.Background(), "alpha", "", 20)
	if err != nil {
		t.Fatalf("ListDeployments: %v", err)
	}
	if len(rows) != 1 || rows[0].ID != "d1" {
		t.Errorf("got %v, want [{ID:d1}]", rows)
	}
}

func TestListDeployments_EnvFilter(t *testing.T) {
	_, rec, rdr := openTestDB(t)
	base := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)

	mustSeed(t, rec, deploy.DeploymentRecord{
		ID: "e1", Project: "p", Env: "staging", EnvType: "local", Mode: "deploy",
		StartedAt: base, ManifestYAML: "kind: Manifest\n", GraphJSON: `{}`,
	}, false, nil)
	mustSeed(t, rec, deploy.DeploymentRecord{
		ID: "e2", Project: "p", Env: "prod", EnvType: "local", Mode: "deploy",
		StartedAt: base.Add(time.Minute), ManifestYAML: "kind: Manifest\n", GraphJSON: `{}`,
	}, false, nil)

	rows, err := rdr.ListDeployments(context.Background(), "", "prod", 20)
	if err != nil {
		t.Fatalf("ListDeployments: %v", err)
	}
	if len(rows) != 1 || rows[0].ID != "e2" {
		t.Errorf("got %v, want [{ID:e2}]", rows)
	}
}

func TestListDeployments_NonexistentFilter(t *testing.T) {
	_, rec, rdr := openTestDB(t)
	mustSeed(t, rec, deploy.DeploymentRecord{
		ID: "x1", Project: "real", Env: "dev", EnvType: "local", Mode: "deploy",
		StartedAt:    time.Now().UTC(),
		ManifestYAML: "kind: Manifest\n", GraphJSON: `{}`,
	}, false, nil)

	rows, err := rdr.ListDeployments(context.Background(), "nonexistent", "", 20)
	if err != nil {
		t.Fatalf("ListDeployments with nonexistent project: %v", err)
	}
	if len(rows) != 0 {
		t.Errorf("expected empty, got %d rows", len(rows))
	}
}

func TestListDeployments_Limit(t *testing.T) {
	_, rec, rdr := openTestDB(t)
	base := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)

	for i := range 5 {
		mustSeed(t, rec, deploy.DeploymentRecord{
			ID: "lim-" + string(rune('a'+i)), Project: "p", Env: "e", EnvType: "local",
			Mode: "deploy", StartedAt: base.Add(time.Duration(i) * time.Hour),
			ManifestYAML: "kind: Manifest\n", GraphJSON: `{}`,
		}, false, nil)
	}

	rows, err := rdr.ListDeployments(context.Background(), "", "", 3)
	if err != nil {
		t.Fatalf("ListDeployments: %v", err)
	}
	if len(rows) != 3 {
		t.Errorf("got %d rows with limit=3, want 3", len(rows))
	}
}

func TestListDeployments_CombinedFilter(t *testing.T) {
	_, rec, rdr := openTestDB(t)
	base := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)

	mustSeed(t, rec, deploy.DeploymentRecord{
		ID: "c1", Project: "p", Env: "dev", EnvType: "local", Mode: "deploy",
		StartedAt: base, ManifestYAML: "kind: Manifest\n", GraphJSON: `{}`,
	}, false, nil)
	mustSeed(t, rec, deploy.DeploymentRecord{
		ID: "c2", Project: "p", Env: "prod", EnvType: "local", Mode: "deploy",
		StartedAt: base.Add(time.Minute), ManifestYAML: "kind: Manifest\n", GraphJSON: `{}`,
	}, false, nil)
	mustSeed(t, rec, deploy.DeploymentRecord{
		ID: "c3", Project: "q", Env: "dev", EnvType: "local", Mode: "deploy",
		StartedAt: base.Add(2 * time.Minute), ManifestYAML: "kind: Manifest\n", GraphJSON: `{}`,
	}, false, nil)

	rows, err := rdr.ListDeployments(context.Background(), "p", "dev", 20)
	if err != nil {
		t.Fatalf("ListDeployments: %v", err)
	}
	if len(rows) != 1 || rows[0].ID != "c1" {
		t.Errorf("got %v, want [{ID:c1}]", rows)
	}
}

// ---------------------------------------------------------------------------
// GetDeployment
// ---------------------------------------------------------------------------

func TestGetDeployment_NotFound(t *testing.T) {
	_, _, rdr := openTestDB(t)
	det, err := rdr.GetDeployment(context.Background(), "no-such-id")
	if err != nil {
		t.Fatalf("GetDeployment: unexpected error: %v", err)
	}
	if det != nil {
		t.Errorf("expected nil, got %+v", det)
	}
}

func TestGetDeployment_FullRecord(t *testing.T) {
	_, rec, rdr := openTestDB(t)
	ctx := context.Background()
	started := time.Date(2026, 3, 15, 9, 0, 0, 0, time.UTC)
	manifest := "kind: Manifest\nmetadata:\n  project: myproj\n"

	mustSeed(t, rec, deploy.DeploymentRecord{
		ID: "full-1", Project: "myproj", Env: "staging", EnvType: "cloud",
		Mode: "deploy", StartedAt: started,
		ManifestYAML: manifest, GraphJSON: `{"nodes":[]}`,
	}, true, []deploy.ResourceRecord{
		{
			ResourceKey: "service|default|api", Type: "service", Class: "default",
			ID: "api", ModuleID: "mod-svc", RunnerID: "local-tofu",
			Status: "applied", OutputsJSON: `{"url":"http://x"}`, LogPath: "/tmp/api.log",
		},
		{
			ResourceKey: "postgres|default|db", Type: "postgres", Class: "default",
			ID: "db", ModuleID: "mod-pg", RunnerID: "local-tofu",
			Status: "applied", LogPath: "/tmp/db.log",
		},
	})

	det, err := rdr.GetDeployment(ctx, "full-1")
	if err != nil {
		t.Fatalf("GetDeployment: %v", err)
	}
	if det == nil {
		t.Fatal("expected non-nil detail")
	}

	if det.ID != "full-1" {
		t.Errorf("ID = %q, want full-1", det.ID)
	}
	if det.Project != "myproj" {
		t.Errorf("Project = %q, want myproj", det.Project)
	}
	if det.Env != "staging" {
		t.Errorf("Env = %q, want staging", det.Env)
	}
	if det.EnvType != "cloud" {
		t.Errorf("EnvType = %q, want cloud", det.EnvType)
	}
	if det.Status != "succeeded" {
		t.Errorf("Status = %q, want succeeded", det.Status)
	}
	if det.Mode != "deploy" {
		t.Errorf("Mode = %q, want deploy", det.Mode)
	}
	if det.ManifestYAML != manifest {
		t.Errorf("ManifestYAML = %q, want %q", det.ManifestYAML, manifest)
	}
	if det.FinishedAt == "" {
		t.Error("FinishedAt should be set after FinishDeployment")
	}
	if len(det.Resources) != 2 {
		t.Fatalf("len(Resources) = %d, want 2", len(det.Resources))
	}
	// resources are ordered by resource_key
	r0 := det.Resources[0]
	if r0.ResourceKey != "postgres|default|db" {
		t.Errorf("Resources[0].ResourceKey = %q, want postgres|default|db", r0.ResourceKey)
	}
	r1 := det.Resources[1]
	if r1.ResourceKey != "service|default|api" {
		t.Errorf("Resources[1].ResourceKey = %q, want service|default|api", r1.ResourceKey)
	}
	if r1.RunnerID != "local-tofu" {
		t.Errorf("Resources[1].RunnerID = %q, want local-tofu", r1.RunnerID)
	}
	if r1.OutputsJSON != `{"url":"http://x"}` {
		t.Errorf("Resources[1].OutputsJSON = %q, want {\"url\":\"http://x\"}", r1.OutputsJSON)
	}
}

func TestGetDeployment_NoResources(t *testing.T) {
	_, rec, rdr := openTestDB(t)
	mustSeed(t, rec, deploy.DeploymentRecord{
		ID: "no-res", Project: "p", Env: "e", EnvType: "local", Mode: "deploy",
		StartedAt:    time.Now().UTC(),
		ManifestYAML: "kind: Manifest\n", GraphJSON: `{}`,
	}, false, nil)

	det, err := rdr.GetDeployment(context.Background(), "no-res")
	if err != nil {
		t.Fatalf("GetDeployment: %v", err)
	}
	if det == nil {
		t.Fatal("expected non-nil")
	}
	if len(det.Resources) != 0 {
		t.Errorf("expected no resources, got %d", len(det.Resources))
	}
}

func TestGetDeployment_FinishedAtEmpty(t *testing.T) {
	_, rec, rdr := openTestDB(t)
	mustSeed(t, rec, deploy.DeploymentRecord{
		ID: "running-1", Project: "p", Env: "e", EnvType: "local", Mode: "deploy",
		StartedAt:    time.Now().UTC(),
		ManifestYAML: "kind: Manifest\n", GraphJSON: `{}`,
	}, false /* not finished */, nil)

	det, err := rdr.GetDeployment(context.Background(), "running-1")
	if err != nil {
		t.Fatalf("GetDeployment: %v", err)
	}
	if det.FinishedAt != "" {
		t.Errorf("FinishedAt = %q, want empty for in-progress deployment", det.FinishedAt)
	}
	if det.Status != "running" {
		t.Errorf("Status = %q, want running", det.Status)
	}
}
