package main

import (
	"bytes"
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/spf13/cobra"
	"gopkg.in/yaml.v3"

	"github.com/krbrudeli/openporch/internal/deploy"
	"github.com/krbrudeli/openporch/internal/store/db"
)

// runGet executes "openporch get <args...> --state-root <stateRoot>" and
// returns captured stdout plus any error.
func runGet(t *testing.T, stateRoot string, args ...string) (string, error) {
	t.Helper()
	root := &cobra.Command{Use: "openporch", SilenceUsage: true, SilenceErrors: true}
	root.AddCommand(newGetCmd())
	var buf bytes.Buffer
	root.SetOut(&buf)
	allArgs := append([]string{"get"}, args...)
	allArgs = append(allArgs, "--state-root", stateRoot)
	root.SetArgs(allArgs)
	err := root.Execute()
	return buf.String(), err
}

// mustSeedDB seeds stateRoot with one complete deployment (started + one
// resource + finished) and returns the deployment ID and manifest YAML.
func mustSeedDB(t *testing.T, stateRoot string) (depID, manifestYAML string) {
	t.Helper()
	d, err := db.Open(stateRoot)
	if err != nil {
		t.Fatalf("db.Open: %v", err)
	}
	defer d.Close()

	ctx := context.Background()
	rec := db.NewRecorder(d)
	depID = "dep-abc123"
	manifestYAML = "kind: Manifest\nmetadata:\n  project: myproj\n"
	started := time.Date(2026, 1, 15, 10, 0, 0, 0, time.UTC)

	if err := rec.StartDeployment(ctx, deploy.DeploymentRecord{
		ID: depID, Project: "myproj", Env: "dev", EnvType: "local",
		Mode: "deploy", StartedAt: started,
		ManifestYAML: manifestYAML, GraphJSON: `{"nodes":[]}`,
	}); err != nil {
		t.Fatalf("StartDeployment: %v", err)
	}
	if err := rec.RecordResource(ctx, depID, deploy.ResourceRecord{
		ResourceKey: "service|default|api", Type: "service", Class: "default",
		ID: "api", ModuleID: "mod-svc", RunnerID: "local-tofu",
		Status: "applied", LogPath: "/tmp/api.log",
	}); err != nil {
		t.Fatalf("RecordResource: %v", err)
	}
	if err := rec.FinishDeployment(ctx, depID, "succeeded", started.Add(2*time.Minute)); err != nil {
		t.Fatalf("FinishDeployment: %v", err)
	}
	return depID, manifestYAML
}

func mustWriteTFPlatform(t *testing.T) string {
	t.Helper()
	root := t.TempDir()
	modDir := filepath.Join(root, "modules", "service")
	if err := os.MkdirAll(modDir, 0o755); err != nil {
		t.Fatalf("mkdir module dir: %v", err)
	}
	if err := os.WriteFile(filepath.Join(modDir, "main.tf"), []byte(`
variable "database_url" {
  type = string
}

output "url" {
  value = var.database_url
}
`), 0o644); err != nil {
		t.Fatalf("write module HCL: %v", err)
	}
	platform := `
apiVersion: openporch/v1alpha1
kind: ResourceType
id: database
output_schema:
  type: object
  properties:
    url:
      type: string
---
apiVersion: openporch/v1alpha1
kind: ResourceType
id: service
output_schema:
  type: object
  properties:
    url:
      type: string
---
apiVersion: openporch/v1alpha1
kind: Module
id: database-mod
resource_type: database
module_source: inline
module_source_code: |
  output "url" {
    value = "postgres://db"
  }
---
apiVersion: openporch/v1alpha1
kind: Module
id: service-mod
resource_type: service
module_source: ./modules/service
module_inputs:
  database_url: ${shared.db.outputs.url}
`
	if err := os.WriteFile(filepath.Join(root, "platform.yaml"), []byte(platform), 0o644); err != nil {
		t.Fatalf("write platform config: %v", err)
	}
	return root
}

func mustSeedTFDeployment(t *testing.T, stateRoot string) string {
	t.Helper()
	d, err := db.Open(stateRoot)
	if err != nil {
		t.Fatalf("db.Open: %v", err)
	}
	defer d.Close()

	ctx := context.Background()
	rec := db.NewRecorder(d)
	depID := "dep-tf123"
	graphJSON := `{"nodes":[` +
		`{"key":"database|default|shared.db","type":"database","class":"default","id":"shared.db","module_id":"database-mod","aliases":["shared.db"]},` +
		`{"key":"service|default|api","type":"service","class":"default","id":"api","module_id":"service-mod","aliases":["workloads.api"],"edges":["database|default|shared.db"]}` +
		`]}`
	started := time.Date(2026, 1, 15, 10, 0, 0, 0, time.UTC)
	if err := rec.StartDeployment(ctx, deploy.DeploymentRecord{
		ID: depID, Project: "myproj", Env: "dev", EnvType: "local",
		Mode: "deploy", StartedAt: started,
		ManifestYAML: "apiVersion: openporch/v1alpha1\nkind: Application\n", GraphJSON: graphJSON,
	}); err != nil {
		t.Fatalf("StartDeployment: %v", err)
	}
	if err := rec.RecordResource(ctx, depID, deploy.ResourceRecord{
		ResourceKey: "database|default|shared.db", Type: "database", Class: "default",
		ID: "shared.db", ModuleID: "database-mod", RunnerID: "local-tofu",
		Status: "applied", OutputsJSON: `{"url":"postgres://db"}`, LogPath: "/tmp/db.log",
	}); err != nil {
		t.Fatalf("RecordResource database: %v", err)
	}
	if err := rec.RecordResource(ctx, depID, deploy.ResourceRecord{
		ResourceKey: "service|default|api", Type: "service", Class: "default",
		ID: "api", ModuleID: "service-mod", RunnerID: "local-tofu",
		Status: "applied", OutputsJSON: `{"url":"http://api"}`, LogPath: "/tmp/api.log",
	}); err != nil {
		t.Fatalf("RecordResource service: %v", err)
	}
	if err := rec.FinishDeployment(ctx, depID, "succeeded", started.Add(2*time.Minute)); err != nil {
		t.Fatalf("FinishDeployment: %v", err)
	}
	return depID
}

// ---------------------------------------------------------------------------
// get deployments
// ---------------------------------------------------------------------------

func TestGetDeployments_EmptyDB(t *testing.T) {
	out, err := runGet(t, t.TempDir(), "deployments")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	// header must appear, no data rows
	if !strings.Contains(out, "ID") || !strings.Contains(out, "PROJECT") {
		t.Errorf("expected table header in output, got:\n%s", out)
	}
	lines := strings.Split(strings.TrimSpace(out), "\n")
	if len(lines) != 1 {
		t.Errorf("expected 1 line (header only), got %d:\n%s", len(lines), out)
	}
}

func TestGetDeployments_ListsDeployment(t *testing.T) {
	root := t.TempDir()
	depID, _ := mustSeedDB(t, root)

	out, err := runGet(t, root, "deployments")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if !strings.Contains(out, depID) {
		t.Errorf("deployment ID %q not found in output:\n%s", depID, out)
	}
	if !strings.Contains(out, "myproj") {
		t.Errorf("project not found in output:\n%s", out)
	}
	if !strings.Contains(out, "succeeded") {
		t.Errorf("status not found in output:\n%s", out)
	}
}

func TestGetDeployments_ProjectFilter(t *testing.T) {
	root := t.TempDir()
	mustSeedDB(t, root)

	// Seed a second deployment under a different project.
	d, _ := db.Open(root)
	rec := db.NewRecorder(d)
	rec.StartDeployment(context.Background(), deploy.DeploymentRecord{
		ID: "dep-other", Project: "other", Env: "dev", EnvType: "local",
		Mode: "deploy", StartedAt: time.Now().UTC(),
		ManifestYAML: "kind: Manifest\n", GraphJSON: `{}`,
	})
	d.Close()

	out, err := runGet(t, root, "deployments", "--project", "myproj")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if !strings.Contains(out, "dep-abc123") {
		t.Errorf("myproj deployment missing from output:\n%s", out)
	}
	if strings.Contains(out, "dep-other") {
		t.Errorf("other project deployment should be filtered out:\n%s", out)
	}
}

func TestGetDeployments_EnvFilter(t *testing.T) {
	root := t.TempDir()
	mustSeedDB(t, root)

	d, err := db.Open(root)
	if err != nil {
		t.Fatalf("db.Open: %v", err)
	}
	rec := db.NewRecorder(d)
	if err := rec.StartDeployment(context.Background(), deploy.DeploymentRecord{
		ID: "dep-prod", Project: "myproj", Env: "prod", EnvType: "remote",
		Mode: "deploy", StartedAt: time.Now().UTC(),
		ManifestYAML: "kind: Manifest\n", GraphJSON: `{}`,
	}); err != nil {
		t.Fatalf("StartDeployment: %v", err)
	}
	d.Close()

	out, err := runGet(t, root, "deployments", "--env", "dev")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if !strings.Contains(out, "dep-abc123") {
		t.Errorf("dev deployment missing from output:\n%s", out)
	}
	if strings.Contains(out, "dep-prod") {
		t.Errorf("prod deployment should be filtered out:\n%s", out)
	}
}

func TestGetDeployments_NonexistentProject(t *testing.T) {
	root := t.TempDir()
	mustSeedDB(t, root)

	out, err := runGet(t, root, "deployments", "--project", "nonexistent")
	if err != nil {
		t.Fatalf("expected no error for nonexistent project, got: %v", err)
	}
	lines := strings.Split(strings.TrimSpace(out), "\n")
	if len(lines) != 1 {
		t.Errorf("expected header only (1 line), got %d lines:\n%s", len(lines), out)
	}
}

func TestGetDeployments_JSONOutput(t *testing.T) {
	root := t.TempDir()
	depID, _ := mustSeedDB(t, root)

	out, err := runGet(t, root, "deployments", "-o", "json")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}

	var rows []db.DeploymentRow
	if err := json.Unmarshal([]byte(out), &rows); err != nil {
		t.Fatalf("output is not valid JSON: %v\n%s", err, out)
	}
	if len(rows) != 1 {
		t.Fatalf("expected 1 row, got %d", len(rows))
	}
	if rows[0].ID != depID {
		t.Errorf("ID = %q, want %q", rows[0].ID, depID)
	}
	if rows[0].Project != "myproj" {
		t.Errorf("Project = %q, want myproj", rows[0].Project)
	}
	if rows[0].Status != "succeeded" {
		t.Errorf("Status = %q, want succeeded", rows[0].Status)
	}
}

func TestGetDeployments_YAMLOutput(t *testing.T) {
	root := t.TempDir()
	depID, _ := mustSeedDB(t, root)

	out, err := runGet(t, root, "deployments", "-o", "yaml")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}

	var rows []db.DeploymentRow
	if err := yaml.Unmarshal([]byte(out), &rows); err != nil {
		t.Fatalf("output is not valid YAML: %v\n%s", err, out)
	}
	if len(rows) != 1 {
		t.Fatalf("expected 1 row, got %d", len(rows))
	}
	if rows[0].ID != depID {
		t.Errorf("ID = %q, want %q", rows[0].ID, depID)
	}
}

func TestGetDeployments_Limit(t *testing.T) {
	root := t.TempDir()
	d, _ := db.Open(root)
	rec := db.NewRecorder(d)
	base := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
	for i := range 5 {
		rec.StartDeployment(context.Background(), deploy.DeploymentRecord{
			ID: "lim-" + string(rune('a'+i)), Project: "p", Env: "e", EnvType: "local",
			Mode: "deploy", StartedAt: base.Add(time.Duration(i) * time.Hour),
			ManifestYAML: "kind: Manifest\n", GraphJSON: `{}`,
		})
	}
	d.Close()

	out, err := runGet(t, root, "deployments", "--limit", "2", "-o", "json")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	var rows []db.DeploymentRow
	if err := json.Unmarshal([]byte(out), &rows); err != nil {
		t.Fatalf("invalid JSON: %v", err)
	}
	if len(rows) != 2 {
		t.Errorf("expected 2 rows with --limit 2, got %d", len(rows))
	}
}

func TestGetDeployments_InvalidOutput(t *testing.T) {
	_, err := runGet(t, t.TempDir(), "deployments", "-o", "xml")
	if err == nil {
		t.Fatal("expected invalid output format error, got nil")
	}
	if !strings.Contains(err.Error(), `unsupported output format "xml"`) {
		t.Fatalf("unexpected error: %v", err)
	}
}

// ---------------------------------------------------------------------------
// get deployment <id>
// ---------------------------------------------------------------------------

func TestGetDeployment_NotFound(t *testing.T) {
	_, err := runGet(t, t.TempDir(), "deployment", "no-such-id")
	if err == nil {
		t.Error("expected error for nonexistent deployment ID, got nil")
	}
}

func TestGetDeployment_DefaultSummary(t *testing.T) {
	root := t.TempDir()
	depID, _ := mustSeedDB(t, root)

	out, err := runGet(t, root, "deployment", depID)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	for _, want := range []string{"ID:", "Project:", "myproj", "Status:", "succeeded", "Mode:", "deploy"} {
		if !strings.Contains(out, want) {
			t.Errorf("expected %q in output:\n%s", want, out)
		}
	}
	// Resources section should appear (we seeded one resource)
	if !strings.Contains(out, "Resources:") {
		t.Errorf("expected Resources section:\n%s", out)
	}
	if !strings.Contains(out, "service|default|api") {
		t.Errorf("expected resource key in output:\n%s", out)
	}
}

func TestGetDeployment_YAMLRoundTrip(t *testing.T) {
	root := t.TempDir()
	depID, manifestYAML := mustSeedDB(t, root)

	out, err := runGet(t, root, "deployment", depID, "-o", "yaml")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if out != manifestYAML {
		t.Errorf("-o yaml output does not round-trip manifest byte-for-byte\ngot:  %q\nwant: %q", out, manifestYAML)
	}
}

func TestGetDeployment_JSONOutput(t *testing.T) {
	root := t.TempDir()
	depID, _ := mustSeedDB(t, root)

	out, err := runGet(t, root, "deployment", depID, "-o", "json")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}

	var det db.DeploymentDetail
	if err := json.Unmarshal([]byte(out), &det); err != nil {
		t.Fatalf("output is not valid JSON: %v\n%s", err, out)
	}
	if det.ID != depID {
		t.Errorf("ID = %q, want %q", det.ID, depID)
	}
	if det.Project != "myproj" {
		t.Errorf("Project = %q, want myproj", det.Project)
	}
	if det.ManifestYAML == "" {
		t.Error("ManifestYAML should be non-empty in JSON output")
	}
	if len(det.Resources) != 1 {
		t.Errorf("expected 1 resource in JSON output, got %d", len(det.Resources))
	}
	if det.Resources[0].RunnerID != "local-tofu" {
		t.Errorf("Resources[0].RunnerID = %q, want local-tofu", det.Resources[0].RunnerID)
	}
}

func TestGetDeployment_InvalidOutput(t *testing.T) {
	root := t.TempDir()
	depID, _ := mustSeedDB(t, root)

	_, err := runGet(t, root, "deployment", depID, "-o", "xml")
	if err == nil {
		t.Fatal("expected invalid output format error, got nil")
	}
	if !strings.Contains(err.Error(), `unsupported output format "xml"`) {
		t.Fatalf("unexpected error: %v", err)
	}
}

// ---------------------------------------------------------------------------
// get tf <deployment-id>
// ---------------------------------------------------------------------------

func TestGetTF_PrintsRenderedHCL(t *testing.T) {
	root := t.TempDir()
	platform := mustWriteTFPlatform(t)
	depID := mustSeedTFDeployment(t, root)

	out, err := runGet(t, root, "tf", depID, "--platform", platform)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	for _, want := range []string{
		"===== database|default|shared.db/main.tf =====",
		"===== service|default|api/main.tf =====",
		`source = "./module"`,
		`source = "` + filepath.Join(platform, "modules", "service") + `"`,
		`database_url = "postgres://db"`,
		`output "url"`,
	} {
		if !strings.Contains(out, want) {
			t.Errorf("expected %q in output:\n%s", want, out)
		}
	}
}

func TestGetTF_OutWritesMainTFPerResource(t *testing.T) {
	root := t.TempDir()
	platform := mustWriteTFPlatform(t)
	depID := mustSeedTFDeployment(t, root)
	outDir := t.TempDir()

	_, err := runGet(t, root, "tf", depID, "--platform", platform, "--out", outDir)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	serviceMain := filepath.Join(outDir, "service|default|api", "main.tf")
	b, err := os.ReadFile(serviceMain)
	if err != nil {
		t.Fatalf("read service main.tf: %v", err)
	}
	if !strings.Contains(string(b), `database_url = "postgres://db"`) {
		t.Errorf("rendered service HCL missing resolved input:\n%s", string(b))
	}
	dbMain := filepath.Join(outDir, "database|default|shared.db", "main.tf")
	if _, err := os.Stat(dbMain); err != nil {
		t.Fatalf("database main.tf not written: %v", err)
	}
	dbModule := filepath.Join(outDir, "database|default|shared.db", "module", "main.tf")
	if _, err := os.Stat(dbModule); err != nil {
		t.Fatalf("inline database module not written: %v", err)
	}
}

func TestGetTF_NotFound(t *testing.T) {
	platform := mustWriteTFPlatform(t)
	_, err := runGet(t, t.TempDir(), "tf", "no-such-id", "--platform", platform)
	if err == nil {
		t.Fatal("expected error for nonexistent deployment ID, got nil")
	}
	if !strings.Contains(err.Error(), `deployment "no-such-id" not found`) {
		t.Fatalf("unexpected error: %v", err)
	}
}
