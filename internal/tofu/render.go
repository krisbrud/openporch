// Package tofu renders an OpenTofu root module for a single resource node.
// One root per resource keeps state files local to that resource and
// sidesteps cross-module locking concerns.
package tofu

import (
	"fmt"
	"regexp"
	"sort"
	"strconv"
	"strings"
)

// ProviderUsage describes one provider that will be emitted as a
// `required_providers` entry plus a `provider` block in the root module.
// Alias is always emitted (even when only one provider of a type is used)
// so module-side `providers = { local = type.alias }` mappings are
// straightforward.
type ProviderUsage struct {
	Type    string         // e.g. "docker", "aws"
	Source  string         // e.g. "kreuzwerker/docker"
	Version string         // optional
	Alias   string         // sanitized provider ID
	Config  map[string]any // resolved provider configuration
}

// Plan is the full input to Render.
type Plan struct {
	ModuleSource    string            // emitted verbatim as the module's `source`
	Inputs          map[string]any    // module-block input arguments
	Providers       []ProviderUsage   // ordered for determinism
	ProviderMapping map[string]string // moduleLocalName -> "<type>.<alias>"
	OutputNames     []string          // emit `output "X" { value = module.main.X }`
}

// Render produces the HCL text of the root main.tf.
func Render(p Plan) (string, error) {
	var sb strings.Builder

	sortProviders(p.Providers)

	sb.WriteString("terraform {\n  required_providers {\n")
	for _, pr := range p.Providers {
		fmt.Fprintf(&sb, "    %s = {\n      source = %q\n", pr.Type, pr.Source)
		if pr.Version != "" {
			fmt.Fprintf(&sb, "      version = %q\n", pr.Version)
		}
		sb.WriteString("    }\n")
	}
	sb.WriteString("  }\n}\n\n")

	for _, pr := range p.Providers {
		fmt.Fprintf(&sb, "provider %q {\n", pr.Type)
		fmt.Fprintf(&sb, "  alias = %q\n", pr.Alias)
		for _, k := range sortedKeys(pr.Config) {
			fmt.Fprintf(&sb, "  %s = %s\n", k, hclValue(pr.Config[k], "  "))
		}
		sb.WriteString("}\n\n")
	}

	fmt.Fprintf(&sb, "module \"main\" {\n  source = %q\n", p.ModuleSource)
	if len(p.ProviderMapping) > 0 {
		sb.WriteString("\n  providers = {\n")
		for _, k := range sortedKeys(p.ProviderMapping) {
			fmt.Fprintf(&sb, "    %s = %s\n", k, p.ProviderMapping[k])
		}
		sb.WriteString("  }\n")
	}
	if len(p.Inputs) > 0 {
		sb.WriteString("\n")
		for _, k := range sortedKeys(p.Inputs) {
			fmt.Fprintf(&sb, "  %s = %s\n", k, hclValue(p.Inputs[k], "  "))
		}
	}
	sb.WriteString("}\n\n")

	sort.Strings(p.OutputNames)
	for _, name := range p.OutputNames {
		fmt.Fprintf(&sb, "output %q {\n  value = module.main.%s\n}\n\n", name, name)
	}
	return sb.String(), nil
}

func hclValue(v any, indent string) string {
	switch x := v.(type) {
	case nil:
		return "null"
	case bool:
		return strconv.FormatBool(x)
	case int:
		return strconv.Itoa(x)
	case int64:
		return strconv.FormatInt(x, 10)
	case float64:
		return strconv.FormatFloat(x, 'g', -1, 64)
	case float32:
		return strconv.FormatFloat(float64(x), 'g', -1, 32)
	case string:
		return strconv.Quote(x)
	case []any:
		if len(x) == 0 {
			return "[]"
		}
		var sb strings.Builder
		sb.WriteString("[\n")
		for i, it := range x {
			sb.WriteString(indent + "  " + hclValue(it, indent+"  "))
			if i < len(x)-1 {
				sb.WriteString(",")
			}
			sb.WriteString("\n")
		}
		sb.WriteString(indent + "]")
		return sb.String()
	case map[string]any:
		if len(x) == 0 {
			return "{}"
		}
		var sb strings.Builder
		sb.WriteString("{\n")
		for _, k := range sortedKeys(x) {
			sb.WriteString(indent + "  " + quoteKey(k) + " = " + hclValue(x[k], indent+"  ") + "\n")
		}
		sb.WriteString(indent + "}")
		return sb.String()
	default:
		return strconv.Quote(fmt.Sprintf("%v", x))
	}
}

var identRE = regexp.MustCompile(`^[A-Za-z_][A-Za-z0-9_-]*$`)

func quoteKey(k string) string {
	if identRE.MatchString(k) {
		return k
	}
	return strconv.Quote(k)
}

func sortedKeys[V any](m map[string]V) []string {
	out := make([]string, 0, len(m))
	for k := range m {
		out = append(out, k)
	}
	sort.Strings(out)
	return out
}

func sortProviders(p []ProviderUsage) {
	sort.Slice(p, func(i, j int) bool {
		if p[i].Type != p[j].Type {
			return p[i].Type < p[j].Type
		}
		return p[i].Alias < p[j].Alias
	})
}
