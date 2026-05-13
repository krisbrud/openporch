package main

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/spf13/cobra"

	"github.com/krbrudeli/openporch/internal/config"
	"github.com/krbrudeli/openporch/internal/deploy"
	"github.com/krbrudeli/openporch/internal/manifest"
	"github.com/krbrudeli/openporch/internal/store"
	"github.com/krbrudeli/openporch/internal/store/db"
)

func newRollbackCmd() *cobra.Command {
	var (
		platformDir string
		envType     string
		stateRoot   string
		tofuBinary  string
		dryRun      bool
		renderDir   string
		planOnly    bool
	)
	cmd := &cobra.Command{
		Use:   "rollback <project> <env> <deployment-id>",
		Short: "Redeploy a past deployment with its original module IDs frozen",
		Long: `rollback replays a past deployment's manifest with module IDs pinned to
the modules that deployment resolved against, bypassing the current module
rules. Provider and runner configuration always come from current platform
state. The new deployment is recorded with mode=rollback.

Use this when module rules have changed since a known-good deployment and
"opo deploy deployment://<id>" would re-pick modules under the new rules.`,
		Args: cobra.ExactArgs(3),
		RunE: func(cmd *cobra.Command, args []string) error {
			project, env, deploymentID := args[0], args[1], args[2]
			if project == "" || env == "" || deploymentID == "" {
				return fmt.Errorf("project, env, and deployment-id are required")
			}

			if dryRun && planOnly {
				return fmt.Errorf("--dry-run and --plan-only are mutually exclusive")
			}

			ctx := context.Background()
			cfg, err := config.Load(platformDir)
			if err != nil {
				return err
			}

			d, err := db.Open(stateRoot)
			if err != nil {
				return err
			}
			defer d.Close()
			rdr := db.NewReader(d)

			det, err := rdr.GetDeployment(ctx, deploymentID)
			if err != nil {
				return err
			}
			if det == nil {
				return fmt.Errorf("deployment %q not found", deploymentID)
			}
			if det.Project != project {
				return fmt.Errorf("deployment %q belongs to project %q, not %q",
					deploymentID, det.Project, project)
			}
			if det.Env != env {
				return fmt.Errorf("deployment %q belongs to env %q, not %q",
					deploymentID, det.Env, env)
			}
			if det.ManifestYAML == "" {
				return fmt.Errorf("deployment %q has no stored manifest", deploymentID)
			}

			m, err := manifest.LoadBytes([]byte(det.ManifestYAML))
			if err != nil {
				return fmt.Errorf("deployment %q: %w", deploymentID, err)
			}

			overrides, err := rdr.GetDeploymentModuleAssignments(ctx, deploymentID)
			if err != nil {
				return err
			}
			if len(overrides) == 0 {
				return fmt.Errorf("deployment %q has no recorded module assignments to roll back to", deploymentID)
			}

			runnerID, r, err := resolveRunner(cfg, project, env, envType, stateRoot, tofuBinary)
			if err != nil {
				return err
			}

			opts := deploy.Options{
				Manifest: m, Platform: cfg, Store: &store.FS{Root: stateRoot},
				Runner: r, RunnerID: runnerID,
				ProjectID: project, EnvID: env, EnvTypeID: envType,
				DryRun: dryRun, RenderDir: renderDir,
				PlanOnly:        planOnly,
				ModuleOverrides: overrides,
				Mode:            "rollback",
			}
			if !dryRun {
				opts.Recorder = db.NewRecorder(d)
			}

			res, err := deploy.Run(ctx, opts)
			if err != nil {
				return err
			}

			out := cmd.OutOrStdout()
			if planOnly {
				fmt.Fprintf(out, "\nRollback plan-only %s complete (status=planned).\n", res.DeploymentID)
				fmt.Fprintln(out, "Run `opo get deployment` to review captured plans.")
				return nil
			}

			if dryRun {
				fmt.Fprintf(out, "Dry-run rollback from %s. %d resource(s) would be deployed:\n\n",
					deploymentID, len(res.DryRunResources))
				for _, r := range res.DryRunResources {
					fmt.Fprintf(out, "  %-40s  module=%-30s  type=%s", r.Key, r.ModuleID, r.Type)
					if r.Class != "" {
						fmt.Fprintf(out, "  class=%s", r.Class)
					}
					if len(r.Providers) > 0 {
						fmt.Fprintf(out, "  providers=%s", strings.Join(r.Providers, ","))
					}
					fmt.Fprintln(out)
				}
				if renderDir != "" {
					fmt.Fprintf(out, "\nRendered HCL written to: %s\n", renderDir)
				}
				return nil
			}

			fmt.Fprintf(out, "\nRollback %s succeeded (from %s).\n\n", res.DeploymentID, deploymentID)
			if len(res.Resolved) > 0 {
				fmt.Fprintln(out, "Resource outputs:")
				keys := make([]string, 0, len(res.Resolved))
				for k := range res.Resolved {
					keys = append(keys, k)
				}
				sort.Strings(keys)
				for _, k := range keys {
					fmt.Fprintf(out, "  %s\n", k)
					oks := make([]string, 0, len(res.Resolved[k]))
					for ok := range res.Resolved[k] {
						oks = append(oks, ok)
					}
					sort.Strings(oks)
					for _, ok := range oks {
						fmt.Fprintf(out, "    %s = %v\n", ok, res.Resolved[k][ok])
					}
				}
			}
			if len(res.Outputs) > 0 {
				fmt.Fprintln(out, "\nManifest outputs:")
				keys := make([]string, 0, len(res.Outputs))
				for k := range res.Outputs {
					keys = append(keys, k)
				}
				sort.Strings(keys)
				for _, k := range keys {
					fmt.Fprintf(out, "  %s = %s\n", k, res.Outputs[k])
				}
			}
			return nil
		},
	}

	cwd, _ := os.Getwd()
	cmd.Flags().StringVar(&platformDir, "platform", filepath.Join(cwd, "platform"),
		"directory holding platform config (resource types, modules, rules, providers)")
	cmd.Flags().StringVar(&envType, "env-type", "local", "environment type (used by runner/provider rules)")
	cmd.Flags().StringVar(&stateRoot, "state-root", ".openporch",
		"directory under which TF state and openporch metadata live")
	cmd.Flags().StringVar(&tofuBinary, "tofu", "", "path to tofu binary (default: $PATH lookup)")
	cmd.Flags().BoolVar(&dryRun, "dry-run", false,
		"validate and render HCL without invoking tofu; prints a per-resource summary")
	cmd.Flags().StringVar(&renderDir, "render-dir", "",
		"with --dry-run: write rendered main.tf files here so you can run `tofu plan` manually")
	cmd.Flags().BoolVar(&planOnly, "plan-only", false,
		"run `tofu init` and `tofu plan` per resource, persisting the plan; no apply is invoked")
	return cmd
}
