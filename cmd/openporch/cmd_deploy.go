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
	"github.com/krbrudeli/openporch/internal/store"
	"github.com/krbrudeli/openporch/internal/store/db"
)

func newDeployCmd() *cobra.Command {
	var (
		platformDir string
		project     string
		env         string
		envType     string
		stateRoot   string
		tofuBinary  string
		dryRun      bool
		renderDir   string
		planOnly    bool
	)
	cmd := &cobra.Command{
		Use:   "deploy <manifest.yaml | deployment://HEAD | deployment://<id> | environment://<env>>",
		Short: "Deploy a manifest end-to-end",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx := context.Background()

			cfg, err := config.Load(platformDir)
			if err != nil {
				return err
			}

			src, err := parseManifestSource(args[0])
			if err != nil {
				return err
			}

			// Open the SQLite store once if either history-source resolution
			// or deployment recording needs it.
			needDB := src.File == "" || !dryRun
			var d *db.DB
			if needDB {
				d, err = db.Open(stateRoot)
				if err != nil {
					return err
				}
				defer d.Close()
			}

			var rdr *db.Reader
			if d != nil {
				rdr = db.NewReader(d)
			}
			m, err := resolveManifestSource(ctx, rdr, src, project, env)
			if err != nil {
				return err
			}

			if project == "" {
				project = m.Metadata.Project
			}
			if project == "" {
				return fmt.Errorf("project not set: pass --project or set metadata.project")
			}

			runnerID, r, err := resolveRunner(cfg, project, env, envType, stateRoot, tofuBinary)
			if err != nil {
				return err
			}

			if dryRun && planOnly {
				return fmt.Errorf("--dry-run and --plan-only are mutually exclusive")
			}
			opts := deploy.Options{
				Manifest: m, Platform: cfg, Store: &store.FS{Root: stateRoot},
				Runner: r, RunnerID: runnerID,
				ProjectID: project, EnvID: env, EnvTypeID: envType,
				DryRun: dryRun, RenderDir: renderDir,
				PlanOnly: planOnly,
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
				fmt.Fprintf(out, "\nPlan-only deployment %s complete (status=planned).\n", res.DeploymentID)
				fmt.Fprintln(out, "Run `opo get deployment` to review captured plans.")
				return nil
			}

			if dryRun {
				fmt.Fprintf(out, "Dry-run complete. %d resource(s) would be deployed:\n\n", len(res.DryRunResources))
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

			fmt.Fprintf(out, "\nDeployment %s succeeded.\n\n", res.DeploymentID)
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
	cmd.Flags().StringVar(&project, "project", "", "project ID (overrides manifest.metadata.project)")
	cmd.Flags().StringVar(&env, "env", "default", "environment ID")
	cmd.Flags().StringVar(&envType, "env-type", "local", "environment type (used by module rules)")
	cmd.Flags().StringVar(&stateRoot, "state-root", ".openporch",
		"directory under which TF state and openporch metadata live")
	cmd.Flags().StringVar(&tofuBinary, "tofu", "", "path to tofu binary (default: $PATH lookup)")
	cmd.Flags().BoolVar(&dryRun, "dry-run", false,
		"validate and render HCL without invoking tofu; prints a per-resource summary")
	cmd.Flags().StringVar(&renderDir, "render-dir", "",
		"with --dry-run: write rendered main.tf files here so you can run `tofu plan` manually")
	cmd.Flags().BoolVar(&planOnly, "plan-only", false,
		"run `tofu init` and `tofu plan` per resource, persisting the plan and a deployment record with mode=plan_only; no apply is invoked")
	return cmd
}
