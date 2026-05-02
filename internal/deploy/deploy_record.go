package deploy

import (
	"encoding/json"

	"github.com/krbrudeli/openporch/internal/graph"
)

// graphSnapshot is the JSON shape persisted to deployment_graph.graph_json.
// It captures the post-resolution graph: every node's identity, its
// resolved module, declared edges, and resolved aliases. It deliberately
// omits Outputs/Status (those live in deployment_resources).
type graphSnapshot struct {
	Nodes []nodeSnapshot `json:"nodes"`
}

type nodeSnapshot struct {
	Key      string         `json:"key"`
	Type     string         `json:"type"`
	Class    string         `json:"class"`
	ID       string         `json:"id"`
	ModuleID string         `json:"module_id,omitempty"`
	Aliases  []string       `json:"aliases,omitempty"`
	Edges    []string       `json:"edges,omitempty"`
	Params   map[string]any `json:"params,omitempty"`
}

// serializeGraph returns the JSON text of the graph snapshot. Errors are
// swallowed at the call site — recording is best-effort.
func serializeGraph(g *graph.Graph) (string, error) {
	if g == nil {
		return "", nil
	}
	snap := graphSnapshot{Nodes: make([]nodeSnapshot, 0, len(g.Nodes))}
	for _, n := range g.Nodes {
		snap.Nodes = append(snap.Nodes, nodeSnapshot{
			Key: n.Key, Type: n.Type, Class: n.Class, ID: n.ID,
			ModuleID: n.ModuleID, Aliases: n.Aliases, Edges: n.Edges,
			Params: n.Params,
		})
	}
	b, err := json.Marshal(snap)
	if err != nil {
		return "", err
	}
	return string(b), nil
}
