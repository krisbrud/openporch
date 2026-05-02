// Package deploy is the deployment pipeline: parse + match + render + apply
// + propagate. The whole v0 demo runs through Run.
package deploy

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"sort"
	"strings"
	"time"

	"github.com/santhosh-tekuri/jsonschema/v5"

	v1 "github.com/krbrudeli/openporch/api/v1alpha1"
	"github.com/krbrudeli/openporch/internal/expr"
	"github.com/krbrudeli/openporch/internal/graph"
	"github.com/krbrudeli/openporch/internal/match"
	"github.com/krbrudeli/openporch/internal/runner"
	"github.com/krbrudeli/openporch/internal/store"
	"github.com/krbrudeli/openporch/internal/tofu"
)

// Options drives a single Run.
type Options struct {
	Manifest     *v1.Manifest
	Platform     *v1.PlatformConfig
	ProjectID    string
	EnvID        string
	EnvTypeID    string
	OrgID        string // optional, surfaced via ${context.org_id}
	Store        *store.FS
	Runner       runner.Runner
	DeploymentID string // identifier used for log paths; auto-set if empty
}

// Result summarises a deploy.
type Result struct {
	DeploymentID string
	Resolved     map[string]map[string]any // graph-key -> outputs
	Outputs      map[string]string         // manifest outputs
}

// envVars implements expr.Vars over os.Getenv with TF_VAR_ prefix. This is
// the minimum needed to support ${var.NAME} for provider configs.
type envVars struct{}

func (envVars) Var(name string) (string, bool) {
	v, ok := os.LookupEnv("TF_VAR_" + name)
	return v, ok
}

type stateView struct {
	g *graph.Graph
}

func (s stateView) AliasOutputs(alias string) (map[string]any, bool) {
	key, ok := s.g.AliasIndex[alias]
	if !ok {
		return nil, false
	}
	n := s.g.Nodes[key]
	if n == nil || n.Outputs == nil {
		return nil, false
	}
	return n.Outputs, true
}

// resolverFromMatcher adapts the rule engine into graph.ModuleResolver.
type resolverFromMatcher struct {
	rules []v1.ModuleRule
	known map[string]v1.ResourceType
	ctx   match.Context
}

func (r resolverFromMatcher) Resolve(n *graph.Node) (string, error) {
	if _, ok := r.known[n.Type]; !ok {
		// Unknown resource type — likely an unintended dep target. Skip.
		return "", graph.ErrSkipModuleResolution
	}
	c := r.ctx
	c.ResourceType = n.Type
	c.ResourceID = n.ID
	c.ResourceClass = n.Class
	id, err := match.Module(r.rules, c)
	if errors.Is(err, match.ErrNoMatch) {
		return "", graph.ErrSkipModuleResolution
	}
	return id, err
}

// Run executes the v0 pipeline end-to-end.
func Run(ctx context.Context, o Options) (*Result, error) {
	if o.DeploymentID == "" {
		o.DeploymentID = time.Now().UTC().Format("20060102T150405Z")
	}
	mctx := match.Context{
		ProjectID: o.ProjectID,
		EnvID:     o.EnvID,
		EnvTypeID: o.EnvTypeID,
	}
	res := resolverFromMatcher{rules: o.Platform.ModuleRules, known: o.Platform.ResourceTypes, ctx: mctx}
	g, err := graph.Build(o.Manifest, o.Platform.Modules, res)
	if err != nil {
		return nil, fmt.Errorf("deploy: build graph: %w", err)
	}

	// Resolve module IDs for any nodes that weren't reached by graph.Build's
	// expansion pass (it only resolves nodes whose deps need expanding;
	// nodes with no deps still need a module).
	for _, n := range g.Nodes {
		if n.ModuleID != "" {
			continue
		}
		modID, err := res.Resolve(n)
		if err != nil {
			if errors.Is(err, graph.ErrSkipModuleResolution) {
				return nil, fmt.Errorf("deploy: no module rule matches %s (type=%s)", n.Key, n.Type)
			}
			return nil, fmt.Errorf("deploy: resolve module for %s: %w", n.Key, err)
		}
		n.ModuleID = modID
	}

	// Validate developer-supplied params against each module's schema.
	if err := validateParams(g, o.Platform.Modules); err != nil {
		return nil, err
	}

	// Topological order.
	ordered, err := g.TopoSort()
	if err != nil {
		return nil, fmt.Errorf("deploy: topo sort: %w", err)
	}

	// Apply each resource in order. v0 = sequential; concurrency-per-wave
	// arrives in v0.5 once we have a second runner type to test against.
	resolved := map[string]map[string]any{}
	for _, n := range ordered {
		mod := o.Platform.Modules[n.ModuleID]
		ectx := expr.Context{
			OrgID: o.OrgID, ProjectID: o.ProjectID, EnvID: o.EnvID,
			EnvTypeID: o.EnvTypeID, ResType: n.Type, ResClass: n.Class, ResID: n.ID,
			WorkloadName: workloadFromAliases(n.Aliases),
		}

		// Merge module_inputs (static) with manifest params (dev-supplied).
		// Manifest params win on key collisions because the module config
		// already forbids same-key in inputs+params at validation time.
		inputs := map[string]any{}
		for k, v := range mod.ModuleInputs {
			inputs[k] = v
		}
		for k, v := range n.Params {
			inputs[k] = v
		}
		st := stateView{g: g}
		resolvedInputs, rerr := expr.ResolveAny(inputs, ectx, st, envVars{})
		if rerr != nil && !errors.Is(rerr, expr.ErrUnresolved) {
			return nil, fmt.Errorf("deploy: resolve inputs for %s: %w", n.Key, rerr)
		}
		if errors.Is(rerr, expr.ErrUnresolved) {
			return nil, fmt.Errorf("deploy: unresolved placeholder in inputs of %s (likely a missing dependency edge): %w",
				n.Key, rerr)
		}

		providers, providerMap, err := assembleProviders(mod, o.Platform.Providers, ectx, st)
		if err != nil {
			return nil, fmt.Errorf("deploy: providers for %s: %w", n.Key, err)
		}

		outNames := outputNamesForType(o.Platform.ResourceTypes[n.Type])

		moduleSource := mod.ModuleSource
		if mod.ModuleSource == "inline" || mod.ModuleSourceCode != "" {
			if err := o.Store.WriteInlineModule(o.ProjectID, o.EnvID, n.Key, mod.ModuleSourceCode); err != nil {
				return nil, err
			}
			moduleSource = "./module"
		} else {
			resolved, err := tofu.ResolveSource(mod.ModuleSource, o.Platform.RootDir)
			if err != nil {
				return nil, fmt.Errorf("deploy: resolve module source for %s: %w", n.Key, err)
			}
			moduleSource = resolved
		}
		hcl, err := tofu.Render(tofu.Plan{
			ModuleSource:    moduleSource,
			Inputs:          resolvedInputs.(map[string]any),
			Providers:       providers,
			ProviderMapping: providerMap,
			OutputNames:     outNames,
		})
		if err != nil {
			return nil, fmt.Errorf("deploy: render %s: %w", n.Key, err)
		}
		workdir, err := o.Store.WriteRootTF(o.ProjectID, o.EnvID, n.Key, hcl)
		if err != nil {
			return nil, err
		}
		log := o.Store.LogFile(o.ProjectID, o.EnvID, n.Key, o.DeploymentID)

		fmt.Fprintf(os.Stderr, "[openporch] applying %s via module=%s\n", n.Key, n.ModuleID)
		n.Status = "applying"
		result, err := o.Runner.Apply(ctx, workdir, log)
		if err != nil {
			n.Status = "failed"
			return nil, fmt.Errorf("deploy: apply %s: %w (see %s)", n.Key, err, log)
		}
		n.Status = "applied"
		n.Outputs = result.Outputs
		resolved[n.Key] = result.Outputs
		if err := o.Store.SaveOutputs(o.ProjectID, o.EnvID, n.Key, result.Outputs); err != nil {
			return nil, err
		}
	}

	// Resolve manifest-level outputs.
	manifestOutputs := map[string]string{}
	for k, v := range o.Manifest.Outputs {
		ectx := expr.Context{
			OrgID: o.OrgID, ProjectID: o.ProjectID, EnvID: o.EnvID, EnvTypeID: o.EnvTypeID,
		}
		s, err := expr.Resolve(v, ectx, stateView{g: g}, envVars{})
		if err != nil {
			return nil, fmt.Errorf("deploy: resolve manifest output %q: %w", k, err)
		}
		manifestOutputs[k] = s
	}

	return &Result{
		DeploymentID: o.DeploymentID,
		Resolved:     resolved,
		Outputs:      manifestOutputs,
	}, nil
}

func validateParams(g *graph.Graph, modules map[string]v1.Module) error {
	for _, n := range g.Nodes {
		mod, ok := modules[n.ModuleID]
		if !ok {
			continue
		}
		schema := mod.ModuleParams
		if len(schema) == 0 || len(n.Params) == 0 {
			continue
		}
		// santhosh-tekuri's v5 wants a compiled schema. Easiest path:
		// marshal schema to JSON, compile in-place.
		c := jsonschema.NewCompiler()
		if err := c.AddResource("schema.json", strings.NewReader(toJSON(schema))); err != nil {
			return fmt.Errorf("deploy: prepare param schema for %s: %w", n.ModuleID, err)
		}
		s, err := c.Compile("schema.json")
		if err != nil {
			return fmt.Errorf("deploy: compile param schema for %s: %w", n.ModuleID, err)
		}
		// Convert n.Params (map[string]any) through the schema.
		if err := s.Validate(any(n.Params)); err != nil {
			return fmt.Errorf("deploy: params for %s (module %s) failed schema: %w",
				n.Key, n.ModuleID, err)
		}
	}
	return nil
}

func workloadFromAliases(aliases []string) string {
	for _, a := range aliases {
		if strings.HasPrefix(a, "workloads.") {
			parts := strings.SplitN(a, ".", 3)
			if len(parts) >= 2 {
				return parts[1]
			}
		}
	}
	return ""
}

func outputNamesForType(rt v1.ResourceType) []string {
	if rt.OutputSchema == nil {
		return nil
	}
	props, _ := rt.OutputSchema["properties"].(map[string]any)
	names := make([]string, 0, len(props))
	for k := range props {
		names = append(names, k)
	}
	sort.Strings(names)
	return names
}

func assembleProviders(
	mod v1.Module, providers map[string]v1.Provider,
	ectx expr.Context, st expr.State,
) ([]tofu.ProviderUsage, map[string]string, error) {
	var out []tofu.ProviderUsage
	mapping := map[string]string{}
	for local, ref := range mod.ProviderMapping {
		// "<provider_type>.<provider_id>"
		parts := strings.SplitN(ref, ".", 2)
		if len(parts) != 2 {
			return nil, nil, fmt.Errorf("invalid provider_mapping value %q", ref)
		}
		ptype, pid := parts[0], parts[1]
		p, ok := providers[pid]
		if !ok {
			return nil, nil, fmt.Errorf("unknown provider %q", pid)
		}
		alias := safeAlias(pid)
		// Resolve placeholders in the provider config.
		cfg, err := expr.ResolveAny(p.Configuration, ectx, st, envVars{})
		if err != nil && !errors.Is(err, expr.ErrUnresolved) {
			return nil, nil, fmt.Errorf("provider %q config: %w", pid, err)
		}
		var cfgMap map[string]any
		if cfg != nil {
			cfgMap, _ = cfg.(map[string]any)
		}
		out = append(out, tofu.ProviderUsage{
			Type: ptype, Source: p.Source, Version: p.VersionConstraint,
			Alias: alias, Config: cfgMap,
		})
		mapping[local] = ptype + "." + alias
	}
	return out, mapping, nil
}

func toJSON(v any) string {
	b, _ := json.Marshal(v)
	return string(b)
}

func safeAlias(id string) string {
	out := make([]byte, 0, len(id))
	for i := 0; i < len(id); i++ {
		c := id[i]
		switch {
		case c >= 'A' && c <= 'Z', c >= 'a' && c <= 'z', c >= '0' && c <= '9', c == '_':
			out = append(out, c)
		default:
			out = append(out, '_')
		}
	}
	return string(out)
}
