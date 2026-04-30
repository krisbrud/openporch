// Package runner is the execution-backend abstraction. v0 ships only
// LocalTofu, which shells out to `tofu` on the host via tfexec.
package runner

import "context"

// Result holds the outputs returned by a successful apply.
type Result struct {
	Outputs map[string]any
}

// Runner executes a rendered root module against a working directory.
type Runner interface {
	// Apply initialises, plans and applies the root module written to
	// `workdir/main.tf` (and optional `workdir/module/`). Stdout/stderr
	// from tofu are streamed to the supplied logfile.
	Apply(ctx context.Context, workdir, logfile string) (*Result, error)

	// Destroy reverses an apply. (Not used in v0 deploy path; reserved.)
	Destroy(ctx context.Context, workdir, logfile string) error
}
