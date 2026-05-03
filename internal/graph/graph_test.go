package graph

import (
	"strings"
	"testing"

	v1 "github.com/krbrudeli/openporch/api/v1alpha1"
)

// fakeResolver returns moduleID only for nodes of the matching resource type,
// otherwise skips. Mirrors the deploy pipeline's behaviour: nodes with no
// module rule simply have no expansion.
type fakeResolver struct {
	moduleID     string
	resourceType string
}

func (f fakeResolver) Resolve(n *Node) (string, error) {
	if n.Type != f.resourceType {
		return "", ErrSkipModuleResolution
	}
	return f.moduleID, nil
}

func TestBuild_workloadAndSharedNodesCreated(t *testing.T) {
	t.Parallel()
	m := &v1.Manifest{
		APIVersion: v1.APIVersion,
		Kind:       v1.KindApplication,
		Metadata:   v1.ManifestMetadata{Name: "app"},
		Workloads: map[string]v1.Workload{
			"api": {
				Type: "workload",
				Resources: map[string]v1.ResourceRef{
					"db": {Type: "postgres"},
				},
			},
		},
		Shared: map[string]v1.ResourceRef{
			"bucket": {Type: "s3"},
		},
	}
	g, err := Build(m, nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := g.AliasIndex["workloads.api"]; !ok {
		t.Errorf("missing workload alias")
	}
	if _, ok := g.AliasIndex["workloads.api.db"]; !ok {
		t.Errorf("missing workload-scoped resource alias")
	}
	if _, ok := g.AliasIndex["shared.bucket"]; !ok {
		t.Errorf("missing shared alias")
	}
}

func TestBuild_dedupesByTypeClassID(t *testing.T) {
	t.Parallel()
	m := &v1.Manifest{
		Workloads: map[string]v1.Workload{
			"a": {
				Type: "workload",
				Resources: map[string]v1.ResourceRef{
					"db": {Type: "postgres", ID: "shared-db"},
				},
			},
			"b": {
				Type: "workload",
				Resources: map[string]v1.ResourceRef{
					"db": {Type: "postgres", ID: "shared-db"},
				},
			},
		},
	}
	g, err := Build(m, nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	count := 0
	for _, n := range g.Nodes {
		if n.Type == "postgres" {
			count++
		}
	}
	if count != 1 {
		t.Fatalf("expected 1 postgres node, got %d", count)
	}
	// Both workloads alias the same node.
	postgresKey := NodeKey("postgres", v1.DefaultClass, "shared-db")
	n := g.Nodes[postgresKey]
	if len(n.Aliases) != 2 {
		t.Errorf("expected 2 aliases, got %v", n.Aliases)
	}
}

func TestBuild_moduleDependenciesExpanded(t *testing.T) {
	t.Parallel()
	mod := v1.Module{
		ID: "postgres-docker", ResourceType: "postgres",
		Dependencies: map[string]v1.Dependency{
			"net": {Type: "docker-network", ID: "@-net"},
		},
	}
	m := &v1.Manifest{
		Workloads: map[string]v1.Workload{
			"a": {Resources: map[string]v1.ResourceRef{"db": {Type: "postgres", ID: "db1"}}},
		},
	}
	g, err := Build(m, map[string]v1.Module{"postgres-docker": mod}, fakeResolver{"postgres-docker", "postgres"})
	if err != nil {
		t.Fatal(err)
	}
	netKey := NodeKey("docker-network", v1.DefaultClass, "db1-net")
	if _, ok := g.Nodes[netKey]; !ok {
		t.Fatalf("expected docker-network node %s, got %v", netKey, mapKeys(g.Nodes))
	}
	dbKey := NodeKey("postgres", v1.DefaultClass, "db1")
	if !contains(g.Nodes[dbKey].Edges, netKey) {
		t.Errorf("expected db to depend on net; edges: %v", g.Nodes[dbKey].Edges)
	}
}

func TestTopoSort_dependenciesBeforeDependents(t *testing.T) {
	t.Parallel()
	g := New()
	a := &Node{Key: "a", Type: "x", Class: "default", ID: "a"}
	b := &Node{Key: "b", Type: "x", Class: "default", ID: "b", Edges: []string{"a"}}
	c := &Node{Key: "c", Type: "x", Class: "default", ID: "c", Edges: []string{"b"}}
	g.Nodes["a"] = a
	g.Nodes["b"] = b
	g.Nodes["c"] = c
	out, err := g.TopoSort()
	if err != nil {
		t.Fatal(err)
	}
	if len(out) != 3 || out[0].Key != "a" || out[1].Key != "b" || out[2].Key != "c" {
		var ks []string
		for _, n := range out {
			ks = append(ks, n.Key)
		}
		t.Fatalf("got order %v want a,b,c", ks)
	}
}

func TestTopoSort_cycleDetected(t *testing.T) {
	t.Parallel()
	g := New()
	g.Nodes["a"] = &Node{Key: "a", Edges: []string{"b"}}
	g.Nodes["b"] = &Node{Key: "b", Edges: []string{"a"}}
	_, err := Build(&v1.Manifest{}, nil, nil) // empty
	if err != nil {
		t.Fatal(err)
	}
	_, err = g.TopoSort()
	if err == nil || !strings.Contains(err.Error(), "cycle") {
		t.Fatalf("expected cycle error, got %v", err)
	}
}

// Regression: cross-recursion between two modules that each depend on the
// other's resource type with an @-suffix expands forever:
//
//	postgres "db1" → cache "db1-c" → postgres "db1-c-p" → cache "db1-c-p-c" → …
//
// The config-time guard only catches a module depending on its OWN type with
// inheriting id; a cross-module ping-pong slips past it, so the runtime
// guard in Build must still fire and now must name the offending tuple.
func TestBuild_runawayExpansionGuarded(t *testing.T) {
	t.Parallel()
	pg := v1.Module{
		ID: "postgres-x", ResourceType: "postgres",
		Dependencies: map[string]v1.Dependency{
			"sidecar": {Type: "cache", ID: "@-c"},
		},
	}
	cache := v1.Module{
		ID: "cache-x", ResourceType: "cache",
		Dependencies: map[string]v1.Dependency{
			"backing": {Type: "postgres", ID: "@-p"},
		},
	}
	mods := map[string]v1.Module{"postgres-x": pg, "cache-x": cache}
	resolver := pingPongResolver{
		byType: map[string]string{"postgres": "postgres-x", "cache": "cache-x"},
	}
	m := &v1.Manifest{
		Workloads: map[string]v1.Workload{
			"a": {Resources: map[string]v1.ResourceRef{"db": {Type: "postgres", ID: "db1"}}},
		},
	}
	_, err := Build(m, mods, resolver)
	if err == nil {
		t.Fatal("expected guard to surface error, got nil")
	}
	if !strings.Contains(err.Error(), "exceeded") {
		t.Fatalf("expected exceeded-passes error, got: %v", err)
	}
	// The error must point at one of the two runaway tuples so users can
	// pinpoint the problem instead of bisecting their platform config.
	msg := err.Error()
	mentionsTuple := (strings.Contains(msg, `"postgres-x"`) && strings.Contains(msg, "type=postgres")) ||
		(strings.Contains(msg, `"cache-x"`) && strings.Contains(msg, "type=cache"))
	if !mentionsTuple {
		t.Fatalf("expected error to name an offending (module, type) tuple, got: %v", err)
	}
}

type pingPongResolver struct{ byType map[string]string }

func (p pingPongResolver) Resolve(n *Node) (string, error) {
	if id, ok := p.byType[n.Type]; ok {
		return id, nil
	}
	return "", ErrSkipModuleResolution
}

func TestResolveID(t *testing.T) {
	t.Parallel()
	cases := []struct{ parent, dep, want string }{
		{"main", "", "main"},
		{"main", "@-primary", "main-primary"},
		{"main", "explicit", "explicit"},
		{"workloads.api.db", "@-replica", "workloads.api.db-replica"},
	}
	for _, c := range cases {
		if got := resolveID(c.parent, c.dep); got != c.want {
			t.Errorf("resolveID(%q,%q) = %q, want %q", c.parent, c.dep, got, c.want)
		}
	}
}

func mapKeys[V any](m map[string]V) []string {
	ks := make([]string, 0, len(m))
	for k := range m {
		ks = append(ks, k)
	}
	return ks
}
