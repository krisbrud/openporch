package tofu

import (
	"strings"
	"testing"

	"github.com/hashicorp/hcl/v2/hclparse"
)

func TestRender_smoke(t *testing.T) {
	t.Parallel()
	p := Plan{
		ModuleSource: "./module",
		Inputs: map[string]any{
			"image": "myorg/api:1.0",
			"env": map[string]any{
				"DATABASE_URL": "postgres://x",
				"PORT":         8080,
			},
		},
		Providers: []ProviderUsage{
			{Type: "docker", Source: "kreuzwerker/docker", Version: "~> 3.0",
				Alias: "default", Config: map[string]any{"host": "unix:///var/run/docker.sock"}},
		},
		ProviderMapping: map[string]string{"docker": "docker.default"},
		OutputNames:     []string{"url", "host"},
	}
	out, err := Render(p)
	if err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{
		`terraform {`,
		`required_providers {`,
		`docker = {`,
		`source = "kreuzwerker/docker"`,
		`version = "~> 3.0"`,
		`provider "docker" {`,
		`alias = "default"`,
		`module "main" {`,
		`source = "./module"`,
		`providers = {`,
		`docker = docker.default`,
		`image = "myorg/api:1.0"`,
		`env = {`,
		`DATABASE_URL = "postgres://x"`,
		`PORT = 8080`,
		`output "host"`,
		`output "url"`,
		`value = module.main.url`,
	} {
		if !strings.Contains(out, want) {
			t.Errorf("rendered output missing %q.\nfull:\n%s", want, out)
		}
	}
}

func TestHclValue_quotesNonIdentKeys(t *testing.T) {
	t.Parallel()
	got := hclValue(map[string]any{"with space": "v"}, "")
	if !strings.Contains(got, `"with space" = "v"`) {
		t.Errorf("got %q", got)
	}
}

// Regression: Render must always emit syntactically valid HCL. Caught
// during smoke-test by tofu init failing on inline `variable "x" { type =
// string, default = "y" }` (single-line block with multiple args). The
// renderer doesn't generate variable blocks, but we still want a
// machine-checked guarantee that what *it* writes parses.
func TestRender_parsesAsValidHCL(t *testing.T) {
	t.Parallel()
	plans := []Plan{
		{ // minimal — no providers, no inputs, no outputs
			ModuleSource: "./module",
		},
		{ // with provider + mapping + nested map input
			ModuleSource: "./mod",
			Inputs: map[string]any{
				"name":  "api",
				"port":  8080,
				"env":   map[string]any{"K": "v", "with-hyphen": "x"},
				"items": []any{"a", "b", 1, true, nil},
			},
			Providers: []ProviderUsage{
				{Type: "docker", Source: "kreuzwerker/docker", Version: "~> 3.0",
					Alias: "default", Config: map[string]any{
						"host": "unix:///var/run/docker.sock",
					}},
				{Type: "aws", Source: "hashicorp/aws", Version: "~> 5.0",
					Alias: "us_east_1", Config: map[string]any{"region": "us-east-1"}},
			},
			ProviderMapping: map[string]string{
				"docker": "docker.default",
				"aws":    "aws.us_east_1",
			},
			OutputNames: []string{"url", "host", "port"},
		},
	}
	for i, p := range plans {
		out, err := Render(p)
		if err != nil {
			t.Fatalf("plan %d render: %v", i, err)
		}
		parser := hclparse.NewParser()
		_, diags := parser.ParseHCL([]byte(out), "rendered.tf")
		if diags.HasErrors() {
			t.Fatalf("plan %d produced invalid HCL:\n%s\n--- diags ---\n%s",
				i, out, diags.Error())
		}
	}
}

func TestRender_deterministicOrder(t *testing.T) {
	t.Parallel()
	p := Plan{
		ModuleSource: "x",
		Inputs:       map[string]any{"b": 1, "a": 2, "c": 3},
		OutputNames:  []string{"z", "a"},
	}
	a, _ := Render(p)
	b, _ := Render(p)
	if a != b {
		t.Errorf("non-deterministic output:\n--A--\n%s\n--B--\n%s", a, b)
	}
}
