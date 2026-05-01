package config

import (
	"fmt"
	"sort"
	"strings"

	"github.com/hashicorp/hcl/v2/hclparse"
	"github.com/hashicorp/hcl/v2/hclsyntax"

	v1 "github.com/krbrudeli/openporch/api/v1alpha1"
)

// validateInlineModuleHCL parses every Module.ModuleSourceCode and accumulates
// HCL diagnostics. Each module ID is iterated in sorted order so that error
// messages are deterministic across runs.
func validateInlineModuleHCL(cfg *v1.PlatformConfig) error {
	ids := make([]string, 0, len(cfg.Modules))
	for id, m := range cfg.Modules {
		if m.ModuleSourceCode != "" {
			ids = append(ids, id)
		}
	}
	sort.Strings(ids)

	for _, id := range ids {
		m := cfg.Modules[id]
		filename := "modules/" + id + ".tf"
		parser := hclparse.NewParser()
		file, diags := parser.ParseHCL([]byte(m.ModuleSourceCode), filename)
		if diags.HasErrors() {
			return fmt.Errorf("config: module %q inline source: %s",
				id, formatHCLDiags(diags))
		}
		if err := checkInlineVarsAgainstInputsAndParams(id, m, file.Body); err != nil {
			return err
		}
	}
	return nil
}

func formatHCLDiags(diags interface{ Error() string }) string {
	// hcl.Diagnostics.Error() already includes file/line/col. Trim the
	// trailing newline some implementations add for readability.
	return strings.TrimRight(diags.Error(), "\n")
}

// checkInlineVarsAgainstInputsAndParams walks the parsed body, finds every
// top-level `variable "<name>"` block, and cross-references the names against
// Module.ModuleInputs and Module.ModuleParams. It surfaces both directions:
// a variable with no default that nothing supplies, and a module_inputs key
// that doesn't correspond to any declared variable.
func checkInlineVarsAgainstInputsAndParams(id string, m v1.Module, body interface{}) error {
	syntaxBody, ok := body.(*hclsyntax.Body)
	if !ok {
		return nil
	}

	declared := map[string]bool{} // name -> hasDefault
	hasDefault := map[string]bool{}
	for _, blk := range syntaxBody.Blocks {
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
		return fmt.Errorf("config: module %q inline source declares variable %q but no module_inputs or module_params property sets it",
			id, name)
	}

	for name := range m.ModuleInputs {
		if !declared[name] {
			return fmt.Errorf("config: module %q module_inputs sets %q but the inline source declares no such variable",
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
