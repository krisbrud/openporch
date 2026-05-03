// Package runnertest provides an in-memory recording implementation of
// runner.Runner for use in unit tests.
package runnertest

import (
	"context"
	"path/filepath"
	"sync"

	"github.com/krbrudeli/openporch/internal/runner"
)

// Call records a single invocation of Apply, Plan, or Destroy.
type Call struct {
	Op      string // "Apply", "Plan", or "Destroy"
	Workdir string
	Logfile string
}

// Recording is a runner.Runner that records every call and returns
// stubbed values. The zero value applies successfully with empty outputs.
//
// Stub fields are keyed by workdir (the path returned by the store for a given
// resource). Use storetest.Fake.ResourceDir to compute the expected key.
type Recording struct {
	mu    sync.Mutex
	Calls []Call

	// ApplyErr, if non-nil, returns the mapped error for the given workdir.
	// A missing key means Apply succeeds.
	ApplyErr map[string]error

	// OutputsByResource returns the outputs map for Apply for the given
	// workdir. A missing key returns an empty map.
	OutputsByResource map[string]map[string]any

	// PlanPathByWorkdir overrides the plan-file path returned by Plan.
	// Defaults to <workdir>/tfplan.bin.
	PlanPathByWorkdir map[string]string

	// PlanErr, if non-nil, returns the mapped error for Plan for the given
	// workdir.
	PlanErr map[string]error

	// DestroyErr, if non-nil, returns the mapped error for Destroy for the
	// given workdir.
	DestroyErr map[string]error
}

func (r *Recording) Apply(ctx context.Context, workdir, logfile string) (*runner.Result, error) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.Calls = append(r.Calls, Call{Op: "Apply", Workdir: workdir, Logfile: logfile})
	if r.ApplyErr != nil {
		if err, ok := r.ApplyErr[workdir]; ok {
			return nil, err
		}
	}
	var outputs map[string]any
	if r.OutputsByResource != nil {
		outputs = r.OutputsByResource[workdir]
	}
	if outputs == nil {
		outputs = map[string]any{}
	}
	return &runner.Result{Outputs: outputs}, nil
}

func (r *Recording) Plan(ctx context.Context, workdir, logfile string) (string, error) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.Calls = append(r.Calls, Call{Op: "Plan", Workdir: workdir, Logfile: logfile})
	if r.PlanErr != nil {
		if err, ok := r.PlanErr[workdir]; ok {
			return "", err
		}
	}
	if r.PlanPathByWorkdir != nil {
		if p, ok := r.PlanPathByWorkdir[workdir]; ok {
			return p, nil
		}
	}
	return filepath.Join(workdir, "tfplan.bin"), nil
}

func (r *Recording) Destroy(ctx context.Context, workdir, logfile string) error {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.Calls = append(r.Calls, Call{Op: "Destroy", Workdir: workdir, Logfile: logfile})
	if r.DestroyErr != nil {
		if err, ok := r.DestroyErr[workdir]; ok {
			return err
		}
	}
	return nil
}
