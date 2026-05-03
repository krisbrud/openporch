package main

import (
	"fmt"
	"os"

	"github.com/spf13/cobra"
)

var version = "v0.0.0-dev"

func main() {
	root := &cobra.Command{
		Use:   "openporch",
		Short: "Open-source platform orchestrator",
		Long: `openporch resolves Score-style application manifests against a
platform-engineer-authored library of OpenTofu modules and rules, picks the
right module for each (project, env, env_type), and runs OpenTofu to converge.`,
		SilenceUsage: true,
	}
	root.AddCommand(newDeployCmd(), newDestroyCmd(), newValidateCmd(), newGetCmd(), newVersionCmd())

	if err := root.Execute(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func newVersionCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "version",
		Short: "Print the openporch version",
		RunE: func(cmd *cobra.Command, args []string) error {
			fmt.Println(version)
			return nil
		},
	}
}
