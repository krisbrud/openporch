package tofu

import (
	"flag"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/google/go-cmp/cmp"
	"github.com/hashicorp/hcl/v2/hclparse"
)

var update = flag.Bool("update", false, "update golden files")

// goldenTest renders p, optionally writes the result to testdata/<name>.golden.hcl
// when -update is set, then asserts byte-for-byte equality with the golden file.
func goldenTest(t *testing.T, p Plan, name string) {
	t.Helper()
	got, err := Render(p)
	if err != nil {
		t.Fatalf("Render: %v", err)
	}
	goldenPath := filepath.Join("testdata", name+".golden.hcl")
	if *update {
		if err := os.MkdirAll(filepath.Dir(goldenPath), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(goldenPath, []byte(got), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	want, err := os.ReadFile(goldenPath)
	if err != nil {
		t.Fatal(err)
	}
	if diff := cmp.Diff(string(want), got); diff != "" {
		t.Errorf("rendered output mismatch (-want +got):\n%s", diff)
	}
}

func TestRender_smoke(t *testing.T) {
	t.Parallel()
	goldenTest(t, Plan{
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
	}, "render_smoke")
}

func TestRender_noProviders(t *testing.T) {
	t.Parallel()
	goldenTest(t, Plan{
		ModuleSource: "./module",
		Inputs:       map[string]any{"name": "api"},
		OutputNames:  []string{"url"},
	}, "render_no_providers")
}

func TestRender_multipleProviders(t *testing.T) {
	t.Parallel()
	goldenTest(t, Plan{
		ModuleSource: "./mod",
		Providers: []ProviderUsage{
			{Type: "aws", Source: "hashicorp/aws", Version: "~> 5.0",
				Alias: "us_east_1", Config: map[string]any{"region": "us-east-1"}},
			{Type: "docker", Source: "kreuzwerker/docker", Version: "~> 3.0",
				Alias: "default", Config: map[string]any{"host": "unix:///var/run/docker.sock"}},
		},
		ProviderMapping: map[string]string{
			"aws":    "aws.us_east_1",
			"docker": "docker.default",
		},
		OutputNames: []string{"endpoint"},
	}, "render_multiple_providers")
}

func TestRender_envNonIdentKeys(t *testing.T) {
	t.Parallel()
	goldenTest(t, Plan{
		ModuleSource: "./module",
		Inputs: map[string]any{
			"env": map[string]any{
				"with-hyphen": "value1",
				"with space":  "value2",
				"NORMAL_KEY":  "value3",
			},
		},
	}, "render_env_non_ident_keys")
}

// TestHclValue_quotesNonIdentKeys checks the quoting property directly — a
// golden diff wouldn't make it obvious that quoting is the invariant being tested.
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
