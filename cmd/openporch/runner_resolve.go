package main

import (
	"fmt"
	"path/filepath"

	v1 "github.com/krbrudeli/openporch/api/v1alpha1"
	"github.com/krbrudeli/openporch/internal/match"
	"github.com/krbrudeli/openporch/internal/runner"
)

// resolveRunner picks the right runner for the given (project, envType) pair
// using runner rules from the platform config.
func resolveRunner(
	cfg *v1.PlatformConfig,
	project, env, envType, stateRoot, binaryPath string,
) (string, runner.Runner, error) {
	ctx := match.Context{
		ProjectID: project,
		EnvID:     env,
		EnvTypeID: envType,
	}
	runnerID, err := match.Runner(cfg.RunnerRules, ctx)
	if err != nil {
		return "", nil, fmt.Errorf("no runner matched (project=%s env=%s env_type=%s): %w",
			project, env, envType, err)
	}
	runnerCfg, ok := cfg.Runners[runnerID]
	if !ok {
		return "", nil, fmt.Errorf("runner rule resolved runner_id=%q but no such runner is defined", runnerID)
	}
	r, err := runner.FromConfig(runnerCfg, binaryPath, filepath.Join(stateRoot, "plugin-cache"))
	if err != nil {
		return "", nil, err
	}
	return runnerID, r, nil
}
