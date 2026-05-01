package deploy

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"time"

	"github.com/krbrudeli/openporch/internal/graph"
	"github.com/krbrudeli/openporch/internal/match"
)

// DestroyOptions drives a single Destroy run. It mirrors Options where
// fields overlap so callers can reuse the same setup for both phases.
type DestroyOptions struct {
	Options
	// Prune removes the resource working directories under
	// <state-root>/state/<project>/<env>/ after a successful destroy.
	// Logs are kept regardless.
	Prune bool
}

// DestroyResult summarises what happened.
type DestroyResult struct {
	DeploymentID string
	Destroyed    []string // resource keys destroyed in order
	Skipped      []string // resource keys with no state on disk
}

// Destroy reverses a previous deploy. It rebuilds the same graph as Run, then
// walks resources in reverse-topo order calling Runner.Destroy on each one
// that has a working directory on disk. Errors per-resource are aggregated;
// every resource is attempted even if an earlier one fails.
func Destroy(ctx context.Context, o DestroyOptions) (*DestroyResult, error) {
	if o.DeploymentID == "" {
		o.DeploymentID = "destroy-" + time.Now().UTC().Format("20060102T150405Z")
	}
	g, err := buildGraph(o.Options)
	if err != nil {
		return nil, fmt.Errorf("destroy: build graph: %w", err)
	}
	ordered, err := g.TopoSort()
	if err != nil {
		return nil, fmt.Errorf("destroy: topo sort: %w", err)
	}
	// Reverse for destroy.
	reversed := make([]*graph.Node, len(ordered))
	for i, n := range ordered {
		reversed[len(ordered)-1-i] = n
	}

	res := &DestroyResult{DeploymentID: o.DeploymentID}
	var firstErr error
	for _, n := range reversed {
		workdir := o.Store.ResourceDir(o.ProjectID, o.EnvID, n.Key)
		if _, statErr := os.Stat(filepath.Join(workdir, "main.tf")); statErr != nil {
			if errors.Is(statErr, os.ErrNotExist) {
				res.Skipped = append(res.Skipped, n.Key)
				continue
			}
			if firstErr == nil {
				firstErr = fmt.Errorf("destroy: stat %s: %w", workdir, statErr)
			}
			continue
		}
		log := o.Store.LogFile(o.ProjectID, o.EnvID, n.Key, o.DeploymentID)
		fmt.Fprintf(os.Stderr, "[openporch] destroying %s\n", n.Key)
		if err := o.Runner.Destroy(ctx, workdir, log); err != nil {
			if firstErr == nil {
				firstErr = fmt.Errorf("destroy: %s: %w (see %s)", n.Key, err, log)
			}
			continue
		}
		res.Destroyed = append(res.Destroyed, n.Key)
		if o.Prune {
			if err := os.RemoveAll(workdir); err != nil && firstErr == nil {
				firstErr = fmt.Errorf("destroy: prune %s: %w", workdir, err)
			}
		}
	}
	return res, firstErr
}

// buildGraph reproduces the graph build logic from Run without applying.
// Both Run and Destroy need the same node set + module resolution.
func buildGraph(o Options) (*graph.Graph, error) {
	mctx := match.Context{
		ProjectID: o.ProjectID,
		EnvID:     o.EnvID,
		EnvTypeID: o.EnvTypeID,
	}
	res := resolverFromMatcher{rules: o.Platform.ModuleRules, known: o.Platform.ResourceTypes, ctx: mctx}
	g, err := graph.Build(o.Manifest, o.Platform.Modules, res)
	if err != nil {
		return nil, err
	}
	for _, n := range g.Nodes {
		if n.ModuleID != "" {
			continue
		}
		modID, err := res.Resolve(n)
		if err != nil {
			if errors.Is(err, graph.ErrSkipModuleResolution) {
				return nil, fmt.Errorf("no module rule matches %s (type=%s)", n.Key, n.Type)
			}
			return nil, fmt.Errorf("resolve module for %s: %w", n.Key, err)
		}
		n.ModuleID = modID
	}
	return g, nil
}
