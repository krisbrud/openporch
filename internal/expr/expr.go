// Package expr resolves placeholder expressions of the form ${path.parts}.
// v0 supports: ${context.*}, ${resources.<alias>.outputs.<key>},
// ${shared.<alias>.outputs.<key>}, ${workloads.<name>.outputs.<key>},
// ${var.NAME}. Unresolved references return ErrUnresolved so the deploy
// pipeline can re-render in a later wave.
package expr

import (
	"errors"
	"fmt"
	"regexp"
	"strings"
)

// ErrUnresolved is returned (wrapped) when a reference points to an output
// that is not yet available. The deploy pipeline treats this as "try
// again in a later wave".
var ErrUnresolved = errors.New("placeholder unresolved")

// Context holds the deploy-wide constants for ${context.*}.
type Context struct {
	OrgID        string
	ProjectID    string
	EnvID        string
	EnvTypeID    string
	ResType      string
	ResClass     string
	ResID        string
	WorkloadName string // local context: name of enclosing workload, if any
}

// AsMap returns the context as a flat map keyed by the suffix after "context.".
func (c Context) AsMap() map[string]any {
	return map[string]any{
		"org_id":      c.OrgID,
		"project_id":  c.ProjectID,
		"env_id":      c.EnvID,
		"env_type_id": c.EnvTypeID,
		"res_type":    c.ResType,
		"res_class":   c.ResClass,
		"res_id":      c.ResID,
		"workload":    c.WorkloadName,
	}
}

// State exposes resource outputs to the evaluator.
type State interface {
	// AliasOutputs returns the outputs map for a manifest-level alias
	// (e.g. "workloads.api.db", "shared.bucket", "workloads.api"), and
	// false if the resource has not yet produced outputs.
	AliasOutputs(alias string) (map[string]any, bool)
}

// Vars is the source for ${var.NAME} (e.g. environment variables under
// TF_VAR_NAME prefix).
type Vars interface {
	Var(name string) (string, bool)
}

var pat = regexp.MustCompile(`\$\{([^}]+)\}`)

// Resolve replaces ${...} placeholders in s. Returns the substituted string
// and ErrUnresolved (wrapped) if any placeholder could not be resolved
// (e.g. an output is not yet available). Unresolved placeholders are left
// in place so a later pass can substitute them.
func Resolve(s string, ctx Context, st State, vars Vars) (string, error) {
	var firstUnresolved error
	out := pat.ReplaceAllStringFunc(s, func(match string) string {
		expr := match[2 : len(match)-1]
		v, err := lookup(expr, ctx, st, vars)
		if err != nil {
			if errors.Is(err, ErrUnresolved) && firstUnresolved == nil {
				firstUnresolved = err
				return match // leave for next pass
			}
			if firstUnresolved == nil {
				firstUnresolved = err
			}
			return match
		}
		return fmt.Sprintf("%v", v)
	})
	return out, firstUnresolved
}

// ResolveAny walks an arbitrary value and resolves every string in place.
// Returns the new value plus the first unresolved/error encountered.
func ResolveAny(v any, ctx Context, st State, vars Vars) (any, error) {
	switch x := v.(type) {
	case string:
		return Resolve(x, ctx, st, vars)
	case map[string]any:
		out := make(map[string]any, len(x))
		var firstErr error
		for k, vv := range x {
			r, err := ResolveAny(vv, ctx, st, vars)
			if err != nil && firstErr == nil {
				firstErr = err
			}
			out[k] = r
		}
		return out, firstErr
	case []any:
		out := make([]any, len(x))
		var firstErr error
		for i, vv := range x {
			r, err := ResolveAny(vv, ctx, st, vars)
			if err != nil && firstErr == nil {
				firstErr = err
			}
			out[i] = r
		}
		return out, firstErr
	default:
		return v, nil
	}
}

func lookup(expr string, ctx Context, st State, vars Vars) (any, error) {
	parts := strings.Split(expr, ".")
	if len(parts) == 0 {
		return nil, fmt.Errorf("empty placeholder")
	}
	switch parts[0] {
	case "context":
		if len(parts) != 2 {
			return nil, fmt.Errorf("context expects exactly one suffix: %s", expr)
		}
		v, ok := ctx.AsMap()[parts[1]]
		if !ok {
			return nil, fmt.Errorf("unknown context key: %s", parts[1])
		}
		return v, nil
	case "var":
		if len(parts) != 2 {
			return nil, fmt.Errorf("var expects exactly one suffix: %s", expr)
		}
		if vars == nil {
			return nil, fmt.Errorf("no var source for %s", expr)
		}
		v, ok := vars.Var(parts[1])
		if !ok {
			return nil, fmt.Errorf("unknown var: %s", parts[1])
		}
		return v, nil
	case "resources":
		// resources.<alias>.outputs.<key>...
		// Within an enclosing workload, this is shorthand for
		// workloads.<workload>.<alias>.outputs.<key>...
		if len(parts) < 4 || parts[2] != "outputs" {
			return nil, fmt.Errorf("malformed resources reference: %s", expr)
		}
		alias := "workloads." + ctx.WorkloadName + "." + parts[1]
		if ctx.WorkloadName == "" {
			alias = parts[1] // best-effort fallback
		}
		return outputLookup(st, alias, parts[3:])
	case "shared":
		if len(parts) < 4 || parts[2] != "outputs" {
			return nil, fmt.Errorf("malformed shared reference: %s", expr)
		}
		return outputLookup(st, "shared."+parts[1], parts[3:])
	case "workloads":
		// workloads.<name>.outputs.<key>...
		if len(parts) < 4 || parts[2] != "outputs" {
			return nil, fmt.Errorf("malformed workloads reference: %s", expr)
		}
		return outputLookup(st, "workloads."+parts[1], parts[3:])
	default:
		return nil, fmt.Errorf("unsupported placeholder root %q in %s", parts[0], expr)
	}
}

func outputLookup(st State, alias string, path []string) (any, error) {
	if st == nil {
		return nil, fmt.Errorf("%w: no state for alias %s", ErrUnresolved, alias)
	}
	outs, ok := st.AliasOutputs(alias)
	if !ok {
		return nil, fmt.Errorf("%w: alias %s has no outputs yet", ErrUnresolved, alias)
	}
	var cur any = outs
	for _, p := range path {
		m, ok := cur.(map[string]any)
		if !ok {
			return nil, fmt.Errorf("path %v in %s: not a map at %q", path, alias, p)
		}
		v, ok := m[p]
		if !ok {
			return nil, fmt.Errorf("%w: %s.%s not present", ErrUnresolved, alias, strings.Join(path, "."))
		}
		cur = v
	}
	return cur, nil
}
