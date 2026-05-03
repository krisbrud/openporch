package db

import (
	"context"
	"database/sql"
	"testing"
	"time"

	"github.com/krbrudeli/openporch/internal/deploy"
)

func TestOpenAppliesSchema(t *testing.T) {
	d, err := Open(t.TempDir())
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer d.Close()

	expected := []string{
		"deployments",
		"deployment_resources",
		"deployment_manifest",
		"deployment_graph",
		"_schema_version",
	}
	for _, name := range expected {
		var got string
		err := d.QueryRow(`SELECT name FROM sqlite_master WHERE type='table' AND name = ?`, name).Scan(&got)
		if err != nil {
			t.Errorf("expected table %s: %v", name, err)
		}
	}

	var version int
	if err := d.QueryRow(`SELECT MAX(version) FROM _schema_version`).Scan(&version); err != nil {
		t.Fatalf("read version: %v", err)
	}
	if version != len(migrations) {
		t.Errorf("schema version = %d, want %d", version, len(migrations))
	}
}

func TestOpenIsIdempotent(t *testing.T) {
	root := t.TempDir()
	d1, err := Open(root)
	if err != nil {
		t.Fatalf("first open: %v", err)
	}
	d1.Close()

	d2, err := Open(root)
	if err != nil {
		t.Fatalf("second open: %v", err)
	}
	defer d2.Close()

	var count int
	if err := d2.QueryRow(`SELECT COUNT(*) FROM _schema_version`).Scan(&count); err != nil {
		t.Fatalf("count versions: %v", err)
	}
	if count != len(migrations) {
		t.Errorf("schema version rows = %d, want %d (migrations were re-applied)", count, len(migrations))
	}
}

func TestRecorderLifecycle(t *testing.T) {
	d, err := Open(t.TempDir())
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer d.Close()

	rec := NewRecorder(d)
	ctx := context.Background()
	started := time.Now().UTC().Truncate(time.Second)

	dep := deploy.DeploymentRecord{
		ID: "dep-1", Project: "myproj", Env: "dev", EnvType: "local",
		Mode: "deploy", StartedAt: started,
		ManifestYAML: "kind: Manifest\n",
		GraphJSON:    `{"nodes":[]}`,
	}
	if err := rec.StartDeployment(ctx, dep); err != nil {
		t.Fatalf("StartDeployment: %v", err)
	}

	var (
		project, env, envType, status, mode string
		finished                             sql.NullString
	)
	if err := d.QueryRow(
		`SELECT project, env, env_type, status, mode, finished_at FROM deployments WHERE id = ?`,
		"dep-1").Scan(&project, &env, &envType, &status, &mode, &finished); err != nil {
		t.Fatalf("read deployment: %v", err)
	}
	if project != "myproj" || env != "dev" || envType != "local" || status != "running" || mode != "deploy" {
		t.Errorf("deployment row mismatch: project=%s env=%s envType=%s status=%s mode=%s",
			project, env, envType, status, mode)
	}
	if finished.Valid {
		t.Errorf("finished_at should be NULL, got %q", finished.String)
	}

	resource := deploy.ResourceRecord{
		ResourceKey: "service|default|api", Type: "service", Class: "default", ID: "api",
		ModuleID: "mod-svc", RunnerID: "local-tofu", Status: "applying",
		LogPath: "/tmp/api.log",
	}
	if err := rec.RecordResource(ctx, "dep-1", resource); err != nil {
		t.Fatalf("RecordResource (insert): %v", err)
	}

	resource.Status = "applied"
	resource.OutputsJSON = `{"url":"http://x"}`
	if err := rec.RecordResource(ctx, "dep-1", resource); err != nil {
		t.Fatalf("RecordResource (update): %v", err)
	}

	var (
		gotStatus  string
		gotOutputs sql.NullString
		rowCount   int
	)
	if err := d.QueryRow(
		`SELECT status, outputs_json FROM deployment_resources WHERE deployment_id = ? AND resource_key = ?`,
		"dep-1", "service|default|api").Scan(&gotStatus, &gotOutputs); err != nil {
		t.Fatalf("read resource: %v", err)
	}
	if gotStatus != "applied" {
		t.Errorf("status = %q, want applied", gotStatus)
	}
	if !gotOutputs.Valid || gotOutputs.String != `{"url":"http://x"}` {
		t.Errorf("outputs = %v, want {\"url\":\"http://x\"}", gotOutputs)
	}
	if err := d.QueryRow(`SELECT COUNT(*) FROM deployment_resources WHERE deployment_id = ?`, "dep-1").Scan(&rowCount); err != nil {
		t.Fatalf("count resources: %v", err)
	}
	if rowCount != 1 {
		t.Errorf("deployment_resources rows = %d, want 1 (upsert duplicated)", rowCount)
	}

	finishedAt := started.Add(2 * time.Minute)
	if err := rec.FinishDeployment(ctx, "dep-1", "succeeded", finishedAt); err != nil {
		t.Fatalf("FinishDeployment: %v", err)
	}

	var finStatus string
	var finFinished sql.NullString
	if err := d.QueryRow(
		`SELECT status, finished_at FROM deployments WHERE id = ?`, "dep-1").Scan(&finStatus, &finFinished); err != nil {
		t.Fatalf("read finished deployment: %v", err)
	}
	if finStatus != "succeeded" {
		t.Errorf("status = %q, want succeeded", finStatus)
	}
	if !finFinished.Valid {
		t.Errorf("finished_at should be set")
	}
}

func TestRecorderDestroyMode(t *testing.T) {
	d, err := Open(t.TempDir())
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer d.Close()
	rec := NewRecorder(d)
	ctx := context.Background()

	if err := rec.StartDeployment(ctx, deploy.DeploymentRecord{
		ID: "des-1", Project: "p", Env: "e", EnvType: "local",
		Mode: "destroy", StartedAt: time.Now().UTC(),
		ManifestYAML: "kind: Manifest\n", GraphJSON: `{}`,
	}); err != nil {
		t.Fatalf("StartDeployment: %v", err)
	}

	var mode string
	if err := d.QueryRow(`SELECT mode FROM deployments WHERE id = ?`, "des-1").Scan(&mode); err != nil {
		t.Fatalf("read: %v", err)
	}
	if mode != "destroy" {
		t.Errorf("mode = %q, want destroy", mode)
	}
}
