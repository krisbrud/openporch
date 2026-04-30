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

// Regression: a module whose dependency regenerates the same resource type
// with @-suffix would, without a guard, expand forever:
//
//	parent="db1" → dep "db1-net" (same type) → "db1-net-net" → …
//
// The fix is the maxPasses cap in Build. This test asserts the cap fires
// and surfaces a meaningful error rather than hanging.
func TestBuild_runawayExpansionGuarded(t *testing.T) {
	// Module says: postgres depends on another postgres at id "@-replica".
	// Each pass produces a brand-new node, so expansion never converges.
	mod := v1.Module{
		ID: "postgres-self-recursive", ResourceType: "postgres",
		Dependencies: map[string]v1.Dependency{
			"replica": {Type: "postgres", ID: "@-r"},
		},
	}
	m := &v1.Manifest{
		Workloads: map[string]v1.Workload{
			"a": {Resources: map[string]v1.ResourceRef{"db": {Type: "postgres", ID: "db1"}}},
		},
	}
	_, err := Build(m, map[string]v1.Module{"postgres-self-recursive": mod},
		fakeResolver{"postgres-self-recursive", "postgres"})
	if err == nil {
		t.Fatal("expected guard to surface error, got nil")
	}
	if !strings.Contains(err.Error(), "exceeded") {
		t.Fatalf("expected exceeded-passes error, got: %v", err)
	}
}

func TestResolveID(t *testing.T) {
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
