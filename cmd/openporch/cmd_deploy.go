package main

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"sort"

	"github.com/spf13/cobra"

	"github.com/krbrudeli/openporch/internal/config"
	"github.com/krbrudeli/openporch/internal/deploy"
	"github.com/krbrudeli/openporch/internal/manifest"
	"github.com/krbrudeli/openporch/internal/runner"
	"github.com/krbrudeli/openporch/internal/store"
)

func newDeployCmd() *cobra.Command {
	var (
		platformDir string
		project     string
		env         string
		envType     string
		stateRoot   string
		tofuBinary  string
	)
	cmd := &cobra.Command{
		Use:   "deploy <manifest.yaml>",
		Short: "Deploy a manifest end-to-end",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx := context.Background()

			cfg, err := config.Load(platformDir)
			if err != nil {
				return err
			}
			m, err := manifest.Load(args[0])
			if err != nil {
				return err
			}
			if project == "" {
				project = m.Metadata.Project
			}
			if project == "" {
				return fmt.Errorf("project not set: pass --project or set metadata.project")
			}

			s := &store.FS{Root: stateRoot}
			r := &runner.LocalTofu{
				BinaryPath:     tofuBinary,
				PluginCacheDir: filepath.Join(stateRoot, "plugin-cache"),
			}

			res, err := deploy.Run(ctx, deploy.Options{
				Manifest: m, Platform: cfg, Store: s, Runner: r,
				ProjectID: project, EnvID: env, EnvTypeID: envType,
			})
			if err != nil {
				return err
			}

			fmt.Printf("\nDeployment %s succeeded.\n\n", res.DeploymentID)
			if len(res.Resolved) > 0 {
				fmt.Println("Resource outputs:")
				keys := make([]string, 0, len(res.Resolved))
				for k := range res.Resolved {
					keys = append(keys, k)
				}
				sort.Strings(keys)
				for _, k := range keys {
					fmt.Printf("  %s\n", k)
					oks := make([]string, 0, len(res.Resolved[k]))
					for ok := range res.Resolved[k] {
						oks = append(oks, ok)
					}
					sort.Strings(oks)
					for _, ok := range oks {
						fmt.Printf("    %s = %v\n", ok, res.Resolved[k][ok])
					}
				}
			}
			if len(res.Outputs) > 0 {
				fmt.Println("\nManifest outputs:")
				keys := make([]string, 0, len(res.Outputs))
				for k := range res.Outputs {
					keys = append(keys, k)
				}
				sort.Strings(keys)
				for _, k := range keys {
					fmt.Printf("  %s = %s\n", k, res.Outputs[k])
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
	return cmd
}
