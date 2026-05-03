package main

import (
	"context"
	"fmt"
	"os"
	"path/filepath"

	"github.com/spf13/cobra"

	"github.com/krbrudeli/openporch/internal/config"
	"github.com/krbrudeli/openporch/internal/deploy"
	"github.com/krbrudeli/openporch/internal/manifest"
	"github.com/krbrudeli/openporch/internal/store"
	"github.com/krbrudeli/openporch/internal/store/db"
)

func newDestroyCmd() *cobra.Command {
	var (
		platformDir string
		project     string
		env         string
		envType     string
		stateRoot   string
		tofuBinary  string
		prune       bool
	)
	cmd := &cobra.Command{
		Use:   "destroy <manifest.yaml>",
		Short: "Tear down a previously deployed manifest",
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

			runnerID, r, err := resolveRunner(cfg, project, env, envType, stateRoot, tofuBinary)
			if err != nil {
				return err
			}
			s := &store.FS{Root: stateRoot}
			d, err := db.Open(stateRoot)
			if err != nil {
				return err
			}
			defer d.Close()

			res, err := deploy.Destroy(ctx, deploy.DestroyOptions{
				Options: deploy.Options{
					Manifest: m, Platform: cfg, Store: s, Runner: r, RunnerID: runnerID,
					ProjectID: project, EnvID: env, EnvTypeID: envType,
					Recorder: db.NewRecorder(d),
				},
				Prune: prune,
			})
			if res != nil {
				if len(res.Destroyed) > 0 {
					fmt.Println("Destroyed:")
					for _, k := range res.Destroyed {
						fmt.Printf("  %s\n", k)
					}
				}
				if len(res.Skipped) > 0 {
					fmt.Println("Skipped (no state on disk):")
					for _, k := range res.Skipped {
						fmt.Printf("  %s\n", k)
					}
				}
			}
			return err
		},
	}

	cwd, _ := os.Getwd()
	cmd.Flags().StringVar(&platformDir, "platform", filepath.Join(cwd, "platform"),
		"directory holding platform config")
	cmd.Flags().StringVar(&project, "project", "", "project ID (overrides manifest.metadata.project)")
	cmd.Flags().StringVar(&env, "env", "default", "environment ID")
	cmd.Flags().StringVar(&envType, "env-type", "local", "environment type")
	cmd.Flags().StringVar(&stateRoot, "state-root", ".openporch",
		"directory under which TF state and openporch metadata live")
	cmd.Flags().StringVar(&tofuBinary, "tofu", "", "path to tofu binary (default: $PATH lookup)")
	cmd.Flags().BoolVar(&prune, "prune", true,
		"remove resource working directories after a successful destroy")
	return cmd
}
