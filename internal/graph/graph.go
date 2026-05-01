// Package graph builds and topologically sorts the resource graph for a
// deployment. Nodes are deduped by the (type, class, id) tuple. Edges:
// dependencies must apply BEFORE the dependent; coprovisioned with
// is_dependent_on_current=true mean the coprovisioned resource must apply
// AFTER the current one.
package graph

import (
	"errors"
	"fmt"
	"sort"
	"strings"

	v1 "github.com/krbrudeli/openporch/api/v1alpha1"
)

// Node is a resource in the graph.
type Node struct {
	Key    string // canonical "type|class|id"
	Type   string
	Class  string
	ID     string
	Params map[string]any

	// Aliases is the set of manifest-level names this node is reachable as.
	// Set during graph build for placeholder resolution.
	// e.g. for a workload-scoped resource: workloads.<workload>.<alias>
	Aliases []string

	// Edges names other nodes (by Key) that must apply before this one.
	Edges []string

	// ModuleID is the resolved module (filled in by the deploy pipeline).
	ModuleID string

	// Outputs is filled after a successful apply.
	Outputs map[string]any

	// Status: pending | applying | applied | failed.
	Status string
}

// NodeKey builds a canonical key from type/class/id, applying defaults.
func NodeKey(typ, class, id string) string {
	if class == "" {
		class = v1.DefaultClass
	}
	return typ + "|" + class + "|" + id
}

// Graph is the deployment graph keyed by canonical key.
type Graph struct {
	Nodes map[string]*Node

	// AliasIndex maps a fully-qualified alias to a node Key.
	// Aliases:
	//   workloads.<workload>          -> the workload node
	//   workloads.<workload>.<name>   -> a workload-scoped resource
	//   shared.<name>                 -> a shared resource
	AliasIndex map[string]string
}

// New creates an empty graph.
func New() *Graph {
	return &Graph{
		Nodes:      map[string]*Node{},
		AliasIndex: map[string]string{},
	}
}

// AddOrMerge inserts a node, merging into an existing one if the key already
// exists (params and aliases are merged; conflicts on params are an error).
func (g *Graph) AddOrMerge(n *Node) error {
	if existing, ok := g.Nodes[n.Key]; ok {
		// Merge aliases.
		for _, a := range n.Aliases {
			if !contains(existing.Aliases, a) {
				existing.Aliases = append(existing.Aliases, a)
			}
		}
		// Merge edges (deduped).
		for _, e := range n.Edges {
			if !contains(existing.Edges, e) {
				existing.Edges = append(existing.Edges, e)
			}
		}
		// Merge params (no conflict tolerated except identical).
		for k, v := range n.Params {
			if ev, exists := existing.Params[k]; exists {
				if !deepEqual(ev, v) {
					return fmt.Errorf("graph: conflicting param %q on resource %s",
						k, n.Key)
				}
			} else {
				if existing.Params == nil {
					existing.Params = map[string]any{}
				}
				existing.Params[k] = v
			}
		}
		return nil
	}
	cp := *n
	g.Nodes[n.Key] = &cp
	for _, a := range n.Aliases {
		g.AliasIndex[a] = n.Key
	}
	return nil
}

// resolveID applies the @-suffix inheritance and default-to-parent rules.
func resolveID(parentID, depID string) string {
	switch {
	case depID == "":
		return parentID
	case strings.HasPrefix(depID, "@"):
		return parentID + depID[1:]
	default:
		return depID
	}
}

// Build constructs the graph from manifest + module registry. Module
// dependencies and coprovisioned resources are expanded recursively.
func Build(m *v1.Manifest, modules map[string]v1.Module, moduleResolver ModuleResolver) (*Graph, error) {
	g := New()

	// Workloads first so their nested resources can reference them.
	for wname, w := range m.Workloads {
		typ := w.Type
		if typ == "" {
			typ = v1.WorkloadType
		}
		class := w.Class
		if class == "" {
			class = v1.DefaultClass
		}
		id := wname
		key := NodeKey(typ, class, id)
		alias := "workloads." + wname
		if err := g.AddOrMerge(&Node{
			Key:     key,
			Type:    typ,
			Class:   class,
			ID:      id,
			Params:  w.Params,
			Aliases: []string{alias},
			Status:  "pending",
		}); err != nil {
			return nil, err
		}

		for rname, r := range w.Resources {
			rclass := r.Class
			if rclass == "" {
				rclass = v1.DefaultClass
			}
			rid := r.ID
			if rid == "" {
				rid = "workloads." + wname + "." + rname
			}
			rkey := NodeKey(r.Type, rclass, rid)
			ralias := "workloads." + wname + "." + rname
			if err := g.AddOrMerge(&Node{
				Key:     rkey,
				Type:    r.Type,
				Class:   rclass,
				ID:      rid,
				Params:  r.Params,
				Aliases: []string{ralias},
				Status:  "pending",
			}); err != nil {
				return nil, err
			}
			// Workload depends on its declared resources (so their outputs
			// are available when the workload module renders).
			g.Nodes[key].Edges = appendUnique(g.Nodes[key].Edges, rkey)
		}
	}

	// Shared.
	for sname, s := range m.Shared {
		sclass := s.Class
		if sclass == "" {
			sclass = v1.DefaultClass
		}
		sid := s.ID
		if sid == "" {
			sid = "shared." + sname
		}
		skey := NodeKey(s.Type, sclass, sid)
		salias := "shared." + sname
		if err := g.AddOrMerge(&Node{
			Key:     skey,
			Type:    s.Type,
			Class:   sclass,
			ID:      sid,
			Params:  s.Params,
			Aliases: []string{salias},
			Status:  "pending",
		}); err != nil {
			return nil, err
		}
	}

	// Expand module-declared deps + coprovisioned for every node currently
	// in the graph. Iterate to a fixed point because expansions can
	// introduce new nodes that themselves need expansion. Guard against
	// runaway recursion (a module that depends on its own resource type
	// with a fresh ID each pass would loop forever otherwise).
	if moduleResolver != nil {
		const maxPasses = 32
		visited := map[string]bool{}
		// expansionCounts tracks how many fresh nodes each (resourceType,
		// moduleID) tuple has produced across passes. When the cap fires we
		// name the worst offender so users can pinpoint a runaway module
		// instead of bisecting a 50-module platform.
		type tuple struct{ resType, moduleID string }
		expansionCounts := map[tuple]int{}
		for pass := 0; pass < maxPasses; pass++ {
			pending := make([]*Node, 0, len(g.Nodes))
			for _, n := range g.Nodes {
				if !visited[n.Key] {
					pending = append(pending, n)
				}
			}
			if len(pending) == 0 {
				break
			}
			for _, n := range pending {
				visited[n.Key] = true
				modID, err := moduleResolver.Resolve(n)
				if err != nil {
					if errors.Is(err, ErrSkipModuleResolution) {
						continue
					}
					return nil, fmt.Errorf("graph: resolving module for %s: %w", n.Key, err)
				}
				n.ModuleID = modID
				mod, ok := modules[modID]
				if !ok {
					return nil, fmt.Errorf("graph: module %q not found", modID)
				}
				before := len(g.Nodes)
				if err := expandDeps(g, n, mod); err != nil {
					return nil, err
				}
				if added := len(g.Nodes) - before; added > 0 {
					expansionCounts[tuple{n.Type, modID}] += added
				}
			}
			if pass == maxPasses-1 {
				var worst tuple
				worstCount := -1
				for t, c := range expansionCounts {
					if c > worstCount {
						worst, worstCount = t, c
					}
				}
				if worstCount > 0 {
					return nil, fmt.Errorf("graph: dependency expansion exceeded %d passes — module %q keeps expanding type=%s each pass (likely a self-referential dependency; check Module.dependencies for IDs starting with \"@\" that resolve back to the same type)",
						maxPasses, worst.moduleID, worst.resType)
				}
				return nil, fmt.Errorf("graph: dependency expansion exceeded %d passes (cycle in module deps?)", maxPasses)
			}
		}
	}

	// Cycle check.
	if cycle := detectCycle(g); cycle != nil {
		return nil, fmt.Errorf("graph: cycle detected: %s", strings.Join(cycle, " -> "))
	}
	return g, nil
}

func expandDeps(g *Graph, parent *Node, mod v1.Module) error {
	for _, dep := range mod.Dependencies {
		class := dep.Class
		if class == "" {
			class = v1.DefaultClass
		}
		id := resolveID(parent.ID, dep.ID)
		key := NodeKey(dep.Type, class, id)
		if err := g.AddOrMerge(&Node{
			Key: key, Type: dep.Type, Class: class, ID: id,
			Params: dep.Params, Status: "pending",
		}); err != nil {
			return err
		}
		// Parent depends on this dep.
		parent.Edges = appendUnique(parent.Edges, key)
	}
	for _, cp := range mod.Coprovisioned {
		class := cp.Class
		if class == "" {
			class = v1.DefaultClass
		}
		id := resolveID(parent.ID, cp.ID)
		key := NodeKey(cp.Type, class, id)
		if err := g.AddOrMerge(&Node{
			Key: key, Type: cp.Type, Class: class, ID: id,
			Params: cp.Params, Status: "pending",
		}); err != nil {
			return err
		}
		if cp.IsDependentOnCurrent {
			// Coprovisioned applies AFTER parent.
			g.Nodes[key].Edges = appendUnique(g.Nodes[key].Edges, parent.Key)
		} else {
			// Default: parent depends on it (applies BEFORE parent).
			parent.Edges = appendUnique(parent.Edges, key)
		}
	}
	return nil
}

// ModuleResolver picks a module for a node. Implemented by the deploy
// package using the rule engine; lifted as an interface so graph stays
// pure-Go-with-no-rule-engine-imports.
type ModuleResolver interface {
	Resolve(n *Node) (string, error)
}

// ErrSkipModuleResolution allows a resolver to defer module resolution
// (e.g. for placeholder-typed resources in future versions).
var ErrSkipModuleResolution = errors.New("skip module resolution")

// TopoSort returns nodes in apply-order (dependencies before dependents).
// Sibling nodes within a wave are sorted by Key for determinism.
func (g *Graph) TopoSort() ([]*Node, error) {
	in := map[string]int{}
	for k, n := range g.Nodes {
		if _, ok := in[k]; !ok {
			in[k] = 0
		}
		for _, e := range n.Edges {
			in[e] += 0 // ensure key present
			_ = e
		}
	}
	for _, n := range g.Nodes {
		for _, e := range n.Edges {
			if _, ok := g.Nodes[e]; !ok {
				return nil, fmt.Errorf("graph: edge to unknown node %q from %q", e, n.Key)
			}
			in[n.Key]++ // n depends on e ⇒ n has incoming edge from e
		}
	}

	// Kahn's algorithm.
	var ready []string
	for k, deg := range in {
		if deg == 0 {
			ready = append(ready, k)
		}
	}
	sort.Strings(ready)

	out := make([]*Node, 0, len(g.Nodes))
	for len(ready) > 0 {
		// Pop in deterministic order.
		head := ready[0]
		ready = ready[1:]
		out = append(out, g.Nodes[head])
		// Decrement everyone who depends on `head`.
		for k, n := range g.Nodes {
			for _, e := range n.Edges {
				if e == head {
					in[k]--
					if in[k] == 0 {
						ready = append(ready, k)
					}
				}
			}
		}
		sort.Strings(ready)
	}
	if len(out) != len(g.Nodes) {
		return nil, errors.New("graph: cycle detected during topo sort")
	}
	return out, nil
}

func detectCycle(g *Graph) []string {
	color := map[string]int{} // 0=white, 1=gray, 2=black
	var stack []string
	var cycle []string
	var visit func(k string) bool
	visit = func(k string) bool {
		switch color[k] {
		case 1:
			// Found a cycle; build it.
			for i, s := range stack {
				if s == k {
					cycle = append([]string{}, stack[i:]...)
					cycle = append(cycle, k)
					return true
				}
			}
			return true
		case 2:
			return false
		}
		color[k] = 1
		stack = append(stack, k)
		for _, e := range g.Nodes[k].Edges {
			if visit(e) {
				return true
			}
		}
		stack = stack[:len(stack)-1]
		color[k] = 2
		return false
	}
	keys := make([]string, 0, len(g.Nodes))
	for k := range g.Nodes {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	for _, k := range keys {
		if color[k] == 0 {
			if visit(k) {
				return cycle
			}
		}
	}
	return nil
}

func appendUnique(s []string, v string) []string {
	for _, x := range s {
		if x == v {
			return s
		}
	}
	return append(s, v)
}

func contains(s []string, v string) bool {
	for _, x := range s {
		if x == v {
			return true
		}
	}
	return false
}

// deepEqual is intentionally narrow: maps and primitives.
func deepEqual(a, b any) bool {
	return fmt.Sprintf("%v", a) == fmt.Sprintf("%v", b)
}
