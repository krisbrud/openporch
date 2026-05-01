package config

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	v1 "github.com/krbrudeli/openporch/api/v1alpha1"
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

// baseCfg returns a PlatformConfig with one ResourceType so module-level
// validation has something to reference. Each test mutates Modules.
func baseCfg() *v1.PlatformConfig {
	return &v1.PlatformConfig{
		ResourceTypes: map[string]v1.ResourceType{
			"postgres": {ID: "postgres"},
		},
		Modules:   map[string]v1.Module{},
		Providers: map[string]v1.Provider{},
		Runners:   map[string]v1.Runner{},
	}
}

func TestValidate_rejectsBadInlineHCL(t *testing.T) {
	cfg := baseCfg()
	cfg.Modules["postgres-bad"] = v1.Module{
		ID: "postgres-bad", ResourceType: "postgres",
		ModuleSource: "inline",
		// Single-line block with multiple arguments — illegal HCL.
		ModuleSourceCode: `variable "x" { type = string, default = "y" }`,
	}
	err := Validate(cfg)
	if err == nil {
		t.Fatal("expected error for malformed inline HCL")
	}
	if !strings.Contains(err.Error(), "postgres-bad") {
		t.Fatalf("error must name the offending module, got: %v", err)
	}
}

func TestValidate_acceptsGoodInlineHCL(t *testing.T) {
	cfg := baseCfg()
	cfg.Modules["postgres-ok"] = v1.Module{
		ID: "postgres-ok", ResourceType: "postgres",
		ModuleSource: "inline",
		ModuleSourceCode: `variable "size" {
  type    = string
  default = "small"
}
variable "res_id" {
  type    = string
  default = "db"
}
output "url" { value = "postgres://localhost/${var.res_id}_${var.size}" }
`,
	}
	if err := Validate(cfg); err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestValidate_inlineVarsMatchInputsAndParams(t *testing.T) {
	t.Run("declared but unset", func(t *testing.T) {
		cfg := baseCfg()
		cfg.Modules["m"] = v1.Module{
			ID: "m", ResourceType: "postgres", ModuleSource: "inline",
			ModuleSourceCode: `variable "missing" { type = string }
`,
		}
		err := Validate(cfg)
		if err == nil || !strings.Contains(err.Error(), "missing") {
			t.Fatalf("want declared-but-unset error mentioning %q, got: %v", "missing", err)
		}
	})
	t.Run("set but undeclared", func(t *testing.T) {
		cfg := baseCfg()
		cfg.Modules["m"] = v1.Module{
			ID: "m", ResourceType: "postgres", ModuleSource: "inline",
			ModuleSourceCode: `variable "size" {
  type    = string
  default = "s"
}
`,
			ModuleInputs: map[string]any{"phantom": "x"},
		}
		err := Validate(cfg)
		if err == nil || !strings.Contains(err.Error(), "phantom") {
			t.Fatalf("want set-but-undeclared error mentioning %q, got: %v", "phantom", err)
		}
	})
	t.Run("satisfied by params property", func(t *testing.T) {
		cfg := baseCfg()
		cfg.Modules["m"] = v1.Module{
			ID: "m", ResourceType: "postgres", ModuleSource: "inline",
			ModuleSourceCode: `variable "image" { type = string }
`,
			ModuleParams: map[string]any{
				"type":       "object",
				"properties": map[string]any{"image": map[string]any{"type": "string"}},
			},
		}
		if err := Validate(cfg); err != nil {
			t.Fatalf("unexpected error: %v", err)
		}
	})
}

func TestValidate_refusesSelfReferentialDep(t *testing.T) {
	cfg := baseCfg()
	cfg.Modules["postgres-self"] = v1.Module{
		ID: "postgres-self", ResourceType: "postgres",
		ModuleSource: "x",
		Dependencies: map[string]v1.Dependency{
			"replica": {Type: "postgres", ID: "@-r"},
		},
	}
	err := Validate(cfg)
	if err == nil {
		t.Fatal("expected error for self-referential dependency")
	}
	if !strings.Contains(err.Error(), "postgres-self") {
		t.Fatalf("error should name the module, got: %v", err)
	}
}

func TestValidate_allowsExplicitSiblingDep(t *testing.T) {
	cfg := baseCfg()
	cfg.Modules["postgres-primary"] = v1.Module{
		ID: "postgres-primary", ResourceType: "postgres",
		ModuleSource: "x",
		Dependencies: map[string]v1.Dependency{
			"replica": {Type: "postgres", ID: "explicit-replica-id"},
		},
	}
	if err := Validate(cfg); err != nil {
		t.Fatalf("unexpected error: %v", err)
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
