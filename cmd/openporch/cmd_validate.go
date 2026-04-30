package main

import (
	"fmt"
	"os"
	"path/filepath"

	"github.com/spf13/cobra"

	"github.com/krbrudeli/openporch/internal/config"
	"github.com/krbrudeli/openporch/internal/manifest"
)

func newValidateCmd() *cobra.Command {
	var platformDir string
	cmd := &cobra.Command{
		Use:   "validate <manifest.yaml>",
		Short: "Validate platform config and a manifest, without applying",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			cfg, err := config.Load(platformDir)
			if err != nil {
				return err
			}
			m, err := manifest.Load(args[0])
			if err != nil {
				return err
			}
			fmt.Printf("platform: %d resource types, %d modules, %d module rules, %d providers, %d runners, %d runner rules\n",
				len(cfg.ResourceTypes), len(cfg.Modules), len(cfg.ModuleRules),
				len(cfg.Providers), len(cfg.Runners), len(cfg.RunnerRules))
			fmt.Printf("manifest: %s (project=%q) — %d workloads, %d shared, %d outputs\n",
				m.Metadata.Name, m.Metadata.Project,
				len(m.Workloads), len(m.Shared), len(m.Outputs))
			return nil
		},
	}
	cwd, _ := os.Getwd()
	cmd.Flags().StringVar(&platformDir, "platform", filepath.Join(cwd, "platform"),
		"directory holding platform config")
	return cmd
}
