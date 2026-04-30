package config

import (
	"os"
	"path/filepath"
	"testing"
)

func writeFile(t *testing.T, dir, name, body string) {
	t.Helper()
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, name), []byte(body), 0o644); err != nil {
		t.Fatal(err)
	}
}

func TestLoad_aggregatesAcrossFiles(t *testing.T) {
	root := t.TempDir()
	writeFile(t, root, "types.yaml", `
apiVersion: openporch/v1alpha1
kind: ResourceType
id: postgres
output_schema:
  type: object
  properties:
    url: { type: string }
---
apiVersion: openporch/v1alpha1
kind: ResourceType
id: workload
output_schema:
  type: object
`)
	writeFile(t, root, "providers.yaml", `
apiVersion: openporch/v1alpha1
kind: Provider
id: docker-default
provider_type: docker
source: kreuzwerker/docker
`)
	writeFile(t, root, "modules.yaml", `
apiVersion: openporch/v1alpha1
kind: Module
id: postgres-docker
resource_type: postgres
module_source: ./modules/postgres-docker
provider_mapping:
  docker: docker.docker-default
`)
	writeFile(t, root, "rules.yaml", `
apiVersion: openporch/v1alpha1
kind: ModuleRule
id: postgres-everywhere
resource_type: postgres
module_id: postgres-docker
`)
	cfg, err := Load(root)
	if err != nil {
		t.Fatal(err)
	}
	if len(cfg.ResourceTypes) != 2 {
		t.Errorf("got %d types, want 2", len(cfg.ResourceTypes))
	}
	if cfg.Modules["postgres-docker"].ResourceType != "postgres" {
		t.Errorf("module not loaded")
	}
	if len(cfg.ModuleRules) != 1 {
		t.Errorf("got %d rules", len(cfg.ModuleRules))
	}
}

func TestLoad_validatesProviderMappingType(t *testing.T) {
	root := t.TempDir()
	writeFile(t, root, "x.yaml", `
apiVersion: openporch/v1alpha1
kind: ResourceType
id: postgres
---
apiVersion: openporch/v1alpha1
kind: Provider
id: aws-default
provider_type: aws
source: hashicorp/aws
---
apiVersion: openporch/v1alpha1
kind: Module
id: bad
resource_type: postgres
module_source: x
provider_mapping:
  docker: docker.aws-default   # mismatch: provider type is aws, not docker
`)
	if _, err := Load(root); err == nil {
		t.Fatal("expected error for provider type mismatch")
	}
}

func TestLoad_unknownKindIsError(t *testing.T) {
	root := t.TempDir()
	writeFile(t, root, "x.yaml", `
apiVersion: openporch/v1alpha1
kind: Banana
id: yellow
`)
	if _, err := Load(root); err == nil {
		t.Fatal("expected error for unknown kind")
	}
}
