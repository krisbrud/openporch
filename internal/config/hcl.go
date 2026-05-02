package config

import (
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/hashicorp/hcl/v2/hclparse"
	"github.com/hashicorp/hcl/v2/hclsyntax"

	v1 "github.com/krbrudeli/openporch/api/v1alpha1"
	"github.com/krbrudeli/openporch/internal/tofu"
)

// validateModuleHCL parses each Module's source HCL — either inline source
// code or the .tf files at a local module_source path — and accumulates HCL
// diagnostics. Each module ID is iterated in sorted order so that error
// messages are deterministic across runs. Remote module sources (git,
// registry, http) are skipped; tofu init catches shape mismatches there.
func validateModuleHCL(cfg *v1.PlatformConfig) error {
	ids := make([]string, 0, len(cfg.Modules))
	for id := range cfg.Modules {
		ids = append(ids, id)
	}
	sort.Strings(ids)

	for _, id := range ids {
		m := cfg.Modules[id]
		bodies, err := parseModuleHCL(cfg, m)
		if err != nil {
			return err
		}
		if bodies == nil {
			continue
		}
		if err := checkVarsAgainstInputsAndParams(id, m, bodies); err != nil {
			return err
		}
	}
	return nil
}

// parseModuleHCL returns the parsed top-level bodies for the module's HCL,
// or (nil, nil) if there's nothing to validate (remote source, no inline
// code).
func parseModuleHCL(cfg *v1.PlatformConfig, m v1.Module) ([]*hclsyntax.Body, error) {
	if m.ModuleSourceCode != "" {
		filename := "modules/" + m.ID + ".tf"
		parser := hclparse.NewParser()
		file, diags := parser.ParseHCL([]byte(m.ModuleSourceCode), filename)
		if diags.HasErrors() {
			return nil, fmt.Errorf("config: module %q inline source: %s",
				m.ID, formatHCLDiags(diags))
		}
		body, ok := file.Body.(*hclsyntax.Body)
		if !ok {
			return nil, nil
		}
		return []*hclsyntax.Body{body}, nil
	}
	if m.ModuleSource == "" || m.ModuleSource == "inline" {
		return nil, nil
	}
	if !tofu.IsLocalSource(m.ModuleSource) {
		return nil, nil
	}
	dir, err := tofu.ResolveSource(m.ModuleSource, cfg.RootDir)
	if err != nil {
		return nil, fmt.Errorf("config: module %q: %w", m.ID, err)
	}
	info, err := os.Stat(dir)
	if err != nil {
		return nil, fmt.Errorf("config: module %q source %q: %w", m.ID, dir, err)
	}
	if !info.IsDir() {
		return nil, fmt.Errorf("config: module %q source %q: not a directory", m.ID, dir)
	}
	entries, err := os.ReadDir(dir)
	if err != nil {
		return nil, fmt.Errorf("config: module %q source %q: %w", m.ID, dir, err)
	}
	var tfFiles []string
	for _, e := range entries {
		if e.IsDir() {
			continue
		}
		name := e.Name()
		if strings.HasSuffix(name, ".tf") {
			tfFiles = append(tfFiles, filepath.Join(dir, name))
		}
	}
	if len(tfFiles) == 0 {
		return nil, fmt.Errorf("config: module %q source %q: no .tf files found", m.ID, dir)
	}
	sort.Strings(tfFiles)
	bodies := make([]*hclsyntax.Body, 0, len(tfFiles))
	for _, path := range tfFiles {
		b, err := os.ReadFile(path)
		if err != nil {
			return nil, fmt.Errorf("config: module %q read %s: %w", m.ID, path, err)
		}
		parser := hclparse.NewParser()
		file, diags := parser.ParseHCL(b, path)
		if diags.HasErrors() {
			return nil, fmt.Errorf("config: module %q source %s: %s",
				m.ID, path, formatHCLDiags(diags))
		}
		if body, ok := file.Body.(*hclsyntax.Body); ok {
			bodies = append(bodies, body)
		}
	}
	return bodies, nil
}

func formatHCLDiags(diags interface{ Error() string }) string {
	// hcl.Diagnostics.Error() already includes file/line/col. Trim the
	// trailing newline some implementations add for readability.
	return strings.TrimRight(diags.Error(), "\n")
}

// checkVarsAgainstInputsAndParams walks the parsed bodies, finds every
// top-level `variable "<name>"` block across all of them, and
// cross-references the names against Module.ModuleInputs and
// Module.ModuleParams. It surfaces both directions: a variable with no
// default that nothing supplies, and a module_inputs key that doesn't
// correspond to any declared variable.
func checkVarsAgainstInputsAndParams(id string, m v1.Module, bodies []*hclsyntax.Body) error {
	declared := map[string]bool{}
	hasDefault := map[string]bool{}
	for _, body := range bodies {
		for _, blk := range body.Blocks {
			if blk.Type != "variable" || len(blk.Labels) != 1 {
				continue
			}
			name := blk.Labels[0]
			declared[name] = true
			if blk.Body != nil {
				if _, ok := blk.Body.Attributes["default"]; ok {
					hasDefault[name] = true
				}
			}
		}
	}

	paramKeys := paramSchemaProperties(m.ModuleParams)

	for name := range declared {
		if hasDefault[name] {
			continue
		}
		if _, ok := m.ModuleInputs[name]; ok {
			continue
		}
		if _, ok := paramKeys[name]; ok {
			continue
		}
		return fmt.Errorf("config: module %q module source declares variable %q but no module_inputs or module_params property sets it",
			id, name)
	}

	for name := range m.ModuleInputs {
		if !declared[name] {
			return fmt.Errorf("config: module %q module_inputs sets %q but the module source declares no such variable",
				id, name)
		}
	}
	return nil
}

// paramSchemaProperties extracts top-level property names from a JSON-schema
// shaped ModuleParams map. The renderer uses module_params as a JSON schema
// (e.g. {type: object, properties: {size: {...}}}); the property names are
// the developer-facing parameter names. A non-schema-shaped map yields no
// keys, suppressing false positives for unconventional shapes.
func paramSchemaProperties(params map[string]any) map[string]struct{} {
	if params == nil {
		return nil
	}
	out := map[string]struct{}{}
	if props, ok := params["properties"].(map[string]any); ok {
		for k := range props {
			out[k] = struct{}{}
		}
		return out
	}
	if props, ok := params["properties"].(map[any]any); ok {
		for k := range props {
			if s, ok := k.(string); ok {
				out[s] = struct{}{}
			}
		}
	}
	return out
}
