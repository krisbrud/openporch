package db

import (
	"context"
	"database/sql"
	"testing"
	"time"

	"github.com/krbrudeli/openporch/internal/deploy"
)

func TestOpenAppliesSchema(t *testing.T) {
	t.Parallel()
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
		"active_resources",
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
	t.Parallel()
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
	t.Parallel()
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
		finished                            sql.NullString
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

func TestRecorderPlanOnly(t *testing.T) {
	t.Parallel()
	d, err := Open(t.TempDir())
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer d.Close()

	rec := NewRecorder(d)
	rdr := NewReader(d)
	ctx := context.Background()
	started := time.Now().UTC().Truncate(time.Second)

	if err := rec.StartDeployment(ctx, deploy.DeploymentRecord{
		ID: "plan-1", Project: "p", Env: "e", EnvType: "local",
		Mode: "plan_only", StartedAt: started,
		ManifestYAML: "kind: Manifest\n", GraphJSON: `{}`,
	}); err != nil {
		t.Fatalf("StartDeployment: %v", err)
	}
	if err := rec.RecordResource(ctx, "plan-1", deploy.ResourceRecord{
		ResourceKey: "service|default|api", Type: "service", Class: "default", ID: "api",
		ModuleID: "mod-svc", RunnerID: "local-tofu", Status: "planned",
		LogPath: "/tmp/api.log", PlanPath: "/tmp/api/tfplan.bin",
	}); err != nil {
		t.Fatalf("RecordResource: %v", err)
	}
	if err := rec.FinishDeployment(ctx, "plan-1", "planned", started.Add(time.Minute)); err != nil {
		t.Fatalf("FinishDeployment: %v", err)
	}

	det, err := rdr.GetDeployment(ctx, "plan-1")
	if err != nil {
		t.Fatalf("GetDeployment: %v", err)
	}
	if det == nil {
		t.Fatal("expected non-nil detail")
	}
	if det.Mode != "plan_only" {
		t.Errorf("Mode = %q, want plan_only", det.Mode)
	}
	if det.Status != "planned" {
		t.Errorf("Status = %q, want planned", det.Status)
	}
	if len(det.Resources) != 1 {
		t.Fatalf("Resources len = %d, want 1", len(det.Resources))
	}
	if got := det.Resources[0].PlanPath; got != "/tmp/api/tfplan.bin" {
		t.Errorf("PlanPath = %q, want /tmp/api/tfplan.bin", got)
	}
	if got := det.Resources[0].Status; got != "planned" {
		t.Errorf("resource Status = %q, want planned", got)
	}
}

func TestRecorderDestroyMode(t *testing.T) {
	t.Parallel()
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

func TestSetActiveResources_UpsertAndReplace(t *testing.T) {
	t.Parallel()
	d, err := Open(t.TempDir())
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer d.Close()
	rec := NewRecorder(d)
	rdr := NewReader(d)
	ctx := context.Background()

	first := []deploy.ActiveResourceRecord{
		{ResourceKey: "service|default|api", Type: "service", Class: "default", ID: "api", ModuleID: "mod-svc", OutputsJSON: `{"url":"http://x"}`},
		{ResourceKey: "postgres|default|db", Type: "postgres", Class: "default", ID: "db", ModuleID: "mod-pg"},
	}
	if err := rec.SetActiveResources(ctx, "proj", "dev", "dep-1", first); err != nil {
		t.Fatalf("SetActiveResources (first): %v", err)
	}

	rows, err := rdr.ListActiveResources(ctx, "proj", "dev")
	if err != nil {
		t.Fatalf("ListActiveResources: %v", err)
	}
	if len(rows) != 2 {
		t.Fatalf("expected 2 rows after first set, got %d", len(rows))
	}
	// ordered by resource_key: postgres first, then service
	if rows[0].ResourceKey != "postgres|default|db" {
		t.Errorf("rows[0].ResourceKey = %q, want postgres|default|db", rows[0].ResourceKey)
	}
	if rows[1].ResourceKey != "service|default|api" {
		t.Errorf("rows[1].ResourceKey = %q, want service|default|api", rows[1].ResourceKey)
	}
	if rows[1].OutputsJSON != `{"url":"http://x"}` {
		t.Errorf("rows[1].OutputsJSON = %q, want {\"url\":\"http://x\"}", rows[1].OutputsJSON)
	}
	if rows[1].DeploymentID != "dep-1" {
		t.Errorf("rows[1].DeploymentID = %q, want dep-1", rows[1].DeploymentID)
	}

	// Second set: removes postgres, keeps service, changes deployment_id.
	second := []deploy.ActiveResourceRecord{
		{ResourceKey: "service|default|api", Type: "service", Class: "default", ID: "api", ModuleID: "mod-svc"},
	}
	if err := rec.SetActiveResources(ctx, "proj", "dev", "dep-2", second); err != nil {
		t.Fatalf("SetActiveResources (second): %v", err)
	}

	rows, err = rdr.ListActiveResources(ctx, "proj", "dev")
	if err != nil {
		t.Fatalf("ListActiveResources after second set: %v", err)
	}
	if len(rows) != 1 {
		t.Fatalf("expected 1 row after second set, got %d", len(rows))
	}
	if rows[0].ResourceKey != "service|default|api" {
		t.Errorf("rows[0].ResourceKey = %q, want service|default|api", rows[0].ResourceKey)
	}
	if rows[0].DeploymentID != "dep-2" {
		t.Errorf("rows[0].DeploymentID = %q, want dep-2", rows[0].DeploymentID)
	}
}

func TestSetActiveResources_IsolatedByProjectEnv(t *testing.T) {
	t.Parallel()
	d, err := Open(t.TempDir())
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer d.Close()
	rec := NewRecorder(d)
	rdr := NewReader(d)
	ctx := context.Background()

	res := []deploy.ActiveResourceRecord{
		{ResourceKey: "service|default|api", Type: "service", Class: "default", ID: "api", ModuleID: "mod-svc"},
	}
	if err := rec.SetActiveResources(ctx, "proj-a", "dev", "dep-a", res); err != nil {
		t.Fatalf("SetActiveResources proj-a: %v", err)
	}
	if err := rec.SetActiveResources(ctx, "proj-b", "dev", "dep-b", res); err != nil {
		t.Fatalf("SetActiveResources proj-b: %v", err)
	}

	// Replacing proj-a/prod should not touch proj-a/dev or proj-b/dev.
	if err := rec.SetActiveResources(ctx, "proj-a", "prod", "dep-c", nil); err != nil {
		t.Fatalf("SetActiveResources proj-a/prod: %v", err)
	}

	rows, err := rdr.ListActiveResources(ctx, "proj-a", "dev")
	if err != nil {
		t.Fatalf("ListActiveResources proj-a/dev: %v", err)
	}
	if len(rows) != 1 {
		t.Errorf("proj-a/dev: expected 1 row, got %d", len(rows))
	}

	rows, err = rdr.ListActiveResources(ctx, "proj-b", "dev")
	if err != nil {
		t.Fatalf("ListActiveResources proj-b/dev: %v", err)
	}
	if len(rows) != 1 {
		t.Errorf("proj-b/dev: expected 1 row, got %d", len(rows))
	}

	rows, err = rdr.ListActiveResources(ctx, "proj-a", "prod")
	if err != nil {
		t.Fatalf("ListActiveResources proj-a/prod: %v", err)
	}
	if len(rows) != 0 {
		t.Errorf("proj-a/prod: expected 0 rows, got %d", len(rows))
	}
}

func TestClearActiveResources(t *testing.T) {
	t.Parallel()
	d, err := Open(t.TempDir())
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer d.Close()
	rec := NewRecorder(d)
	rdr := NewReader(d)
	ctx := context.Background()

	res := []deploy.ActiveResourceRecord{
		{ResourceKey: "service|default|api", Type: "service", Class: "default", ID: "api", ModuleID: "mod-svc"},
	}
	if err := rec.SetActiveResources(ctx, "proj", "dev", "dep-1", res); err != nil {
		t.Fatalf("SetActiveResources: %v", err)
	}
	if err := rec.SetActiveResources(ctx, "proj", "prod", "dep-2", res); err != nil {
		t.Fatalf("SetActiveResources prod: %v", err)
	}

	if err := rec.ClearActiveResources(ctx, "proj", "dev"); err != nil {
		t.Fatalf("ClearActiveResources: %v", err)
	}

	rows, err := rdr.ListActiveResources(ctx, "proj", "dev")
	if err != nil {
		t.Fatalf("ListActiveResources dev after clear: %v", err)
	}
	if len(rows) != 0 {
		t.Errorf("expected 0 rows after clear, got %d", len(rows))
	}

	// prod must be untouched.
	rows, err = rdr.ListActiveResources(ctx, "proj", "prod")
	if err != nil {
		t.Fatalf("ListActiveResources prod: %v", err)
	}
	if len(rows) != 1 {
		t.Errorf("prod: expected 1 row, got %d", len(rows))
	}
}
