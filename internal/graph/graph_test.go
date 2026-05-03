package graph

import (
	"strings"
	"testing"

	"github.com/google/go-cmp/cmp"
	"github.com/google/go-cmp/cmp/cmpopts"
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
	want := &Graph{
		Nodes: map[string]*Node{
			NodeKey("workload", v1.DefaultClass, "api"): {
				Key:     NodeKey("workload", v1.DefaultClass, "api"),
				Type:    "workload",
				Class:   v1.DefaultClass,
				ID:      "api",
				Aliases: []string{"workloads.api"},
				Edges:   []string{NodeKey("postgres", v1.DefaultClass, "workloads.api.db")},
				Status:  "pending",
			},
			NodeKey("postgres", v1.DefaultClass, "workloads.api.db"): {
				Key:     NodeKey("postgres", v1.DefaultClass, "workloads.api.db"),
				Type:    "postgres",
				Class:   v1.DefaultClass,
				ID:      "workloads.api.db",
				Aliases: []string{"workloads.api.db"},
				Status:  "pending",
			},
			NodeKey("s3", v1.DefaultClass, "shared.bucket"): {
				Key:     NodeKey("s3", v1.DefaultClass, "shared.bucket"),
				Type:    "s3",
				Class:   v1.DefaultClass,
				ID:      "shared.bucket",
				Aliases: []string{"shared.bucket"},
				Status:  "pending",
			},
		},
		AliasIndex: map[string]string{
			"workloads.api":    NodeKey("workload", v1.DefaultClass, "api"),
			"workloads.api.db": NodeKey("postgres", v1.DefaultClass, "workloads.api.db"),
			"shared.bucket":    NodeKey("s3", v1.DefaultClass, "shared.bucket"),
		},
	}
	if diff := graphDiff(want, g); diff != "" {
		t.Errorf("Build() mismatch (-want +got):\n%s", diff)
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
	postgresKey := NodeKey("postgres", v1.DefaultClass, "shared-db")
	want := &Graph{
		Nodes: map[string]*Node{
			NodeKey("workload", v1.DefaultClass, "a"): {
				Key:     NodeKey("workload", v1.DefaultClass, "a"),
				Type:    "workload",
				Class:   v1.DefaultClass,
				ID:      "a",
				Aliases: []string{"workloads.a"},
				Edges:   []string{postgresKey},
				Status:  "pending",
			},
			NodeKey("workload", v1.DefaultClass, "b"): {
				Key:     NodeKey("workload", v1.DefaultClass, "b"),
				Type:    "workload",
				Class:   v1.DefaultClass,
				ID:      "b",
				Aliases: []string{"workloads.b"},
				Edges:   []string{postgresKey},
				Status:  "pending",
			},
			postgresKey: {
				Key:     postgresKey,
				Type:    "postgres",
				Class:   v1.DefaultClass,
				ID:      "shared-db",
				Aliases: []string{"workloads.a.db", "workloads.b.db"},
				Status:  "pending",
			},
		},
		AliasIndex: map[string]string{
			"workloads.a":    NodeKey("workload", v1.DefaultClass, "a"),
			"workloads.a.db": postgresKey,
			"workloads.b":    NodeKey("workload", v1.DefaultClass, "b"),
			"workloads.b.db": postgresKey,
		},
	}
	if diff := graphDiff(want, g); diff != "" {
		t.Errorf("Build() mismatch (-want +got):\n%s", diff)
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
	dbKey := NodeKey("postgres", v1.DefaultClass, "db1")
	workloadKey := NodeKey("workload", v1.DefaultClass, "a")
	want := &Graph{
		Nodes: map[string]*Node{
			workloadKey: {
				Key:     workloadKey,
				Type:    "workload",
				Class:   v1.DefaultClass,
				ID:      "a",
				Aliases: []string{"workloads.a"},
				Edges:   []string{dbKey},
				Status:  "pending",
			},
			dbKey: {
				Key:      dbKey,
				Type:     "postgres",
				Class:    v1.DefaultClass,
				ID:       "db1",
				Aliases:  []string{"workloads.a.db"},
				Edges:    []string{netKey},
				ModuleID: "postgres-docker",
				Status:   "pending",
			},
			netKey: {
				Key:    netKey,
				Type:   "docker-network",
				Class:  v1.DefaultClass,
				ID:     "db1-net",
				Status: "pending",
			},
		},
		AliasIndex: map[string]string{
			"workloads.a":    workloadKey,
			"workloads.a.db": dbKey,
		},
	}
	if diff := graphDiff(want, g); diff != "" {
		t.Errorf("Build() mismatch (-want +got):\n%s", diff)
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
	var got []string
	for _, n := range out {
		got = append(got, n.Key)
	}
	want := []string{"a", "b", "c"}
	if diff := cmp.Diff(want, got); diff != "" {
		t.Fatalf("TopoSort() mismatch (-want +got):\n%s", diff)
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

// Regression: cross-recursion between two modules that each depend on the
// other's resource type with an @-suffix expands forever:
//
//	postgres "db1" → cache "db1-c" → postgres "db1-c-p" → cache "db1-c-p-c" → …
//
// The config-time guard only catches a module depending on its OWN type with
// inheriting id; a cross-module ping-pong slips past it, so the runtime
// guard in Build must still fire and now must name the offending tuple.
func TestBuild_runawayExpansionGuarded(t *testing.T) {
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

func graphDiff(want, got *Graph) string {
	return cmp.Diff(want, got, cmpopts.SortSlices(func(a, b string) bool {
		return a < b
	}))
}
