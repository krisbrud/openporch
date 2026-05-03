package main

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"text/tabwriter"

	"github.com/spf13/cobra"
	"gopkg.in/yaml.v3"

	"github.com/krbrudeli/openporch/internal/store/db"
)

func newGetCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "get",
		Short: "Read deployment history",
	}
	cmd.AddCommand(newGetDeploymentsCmd(), newGetDeploymentCmd())
	return cmd
}

func newGetDeploymentsCmd() *cobra.Command {
	var (
		project   string
		env       string
		limit     int
		output    string
		stateRoot string
	)
	cmd := &cobra.Command{
		Use:   "deployments",
		Short: "List deployment history",
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx := context.Background()
			d, err := db.Open(stateRoot)
			if err != nil {
				return err
			}
			defer d.Close()

			rows, err := db.NewReader(d).ListDeployments(ctx, project, env, limit)
			if err != nil {
				return err
			}

			switch output {
			case "json":
				enc := json.NewEncoder(os.Stdout)
				enc.SetIndent("", "  ")
				return enc.Encode(rows)
			case "yaml":
				enc := yaml.NewEncoder(os.Stdout)
				enc.SetIndent(2)
				return enc.Encode(rows)
			default:
				w := tabwriter.NewWriter(os.Stdout, 0, 0, 2, ' ', 0)
				fmt.Fprintln(w, "ID\tPROJECT\tENV\tSTATUS\tSTARTED_AT\tMODE")
				for _, row := range rows {
					fmt.Fprintf(w, "%s\t%s\t%s\t%s\t%s\t%s\n",
						row.ID, row.Project, row.Env, row.Status, row.StartedAt, row.Mode)
				}
				return w.Flush()
			}
		},
	}
	cmd.Flags().StringVar(&project, "project", "", "filter by project")
	cmd.Flags().StringVar(&env, "env", "", "filter by environment")
	cmd.Flags().IntVar(&limit, "limit", 20, "maximum number of results")
	cmd.Flags().StringVarP(&output, "output", "o", "table", "output format: table, yaml, json")
	cmd.Flags().StringVar(&stateRoot, "state-root", ".openporch",
		"directory under which openporch metadata lives")
	return cmd
}

func newGetDeploymentCmd() *cobra.Command {
	var (
		output    string
		stateRoot string
	)
	cmd := &cobra.Command{
		Use:   "deployment <id>",
		Short: "Get a single deployment record",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx := context.Background()
			d, err := db.Open(stateRoot)
			if err != nil {
				return err
			}
			defer d.Close()

			det, err := db.NewReader(d).GetDeployment(ctx, args[0])
			if err != nil {
				return err
			}
			if det == nil {
				return fmt.Errorf("deployment %q not found", args[0])
			}

			switch output {
			case "yaml":
				fmt.Print(det.ManifestYAML)
				return nil
			case "json":
				enc := json.NewEncoder(os.Stdout)
				enc.SetIndent("", "  ")
				return enc.Encode(det)
			default:
				fmt.Printf("ID:         %s\n", det.ID)
				fmt.Printf("Project:    %s\n", det.Project)
				fmt.Printf("Env:        %s\n", det.Env)
				fmt.Printf("EnvType:    %s\n", det.EnvType)
				fmt.Printf("Status:     %s\n", det.Status)
				fmt.Printf("Mode:       %s\n", det.Mode)
				fmt.Printf("StartedAt:  %s\n", det.StartedAt)
				if det.FinishedAt != "" {
					fmt.Printf("FinishedAt: %s\n", det.FinishedAt)
				}
				if len(det.Resources) > 0 {
					fmt.Println("\nResources:")
					w := tabwriter.NewWriter(os.Stdout, 0, 0, 2, ' ', 0)
					fmt.Fprintln(w, "  KEY\tTYPE\tSTATUS\tRUNNER\tLOG")
					for _, res := range det.Resources {
						fmt.Fprintf(w, "  %s\t%s\t%s\t%s\t%s\n",
							res.ResourceKey, res.Type, res.Status, res.RunnerID, res.LogPath)
					}
					w.Flush()
				}
				return nil
			}
		},
	}
	cmd.Flags().StringVarP(&output, "output", "o", "", "output format: yaml, json (default: human summary)")
	cmd.Flags().StringVar(&stateRoot, "state-root", ".openporch",
		"directory under which openporch metadata lives")
	return cmd
}
