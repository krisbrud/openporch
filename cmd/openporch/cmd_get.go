package main

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
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

			out := cmd.OutOrStdout()
			switch output {
			case "json":
				enc := json.NewEncoder(out)
				enc.SetIndent("", "  ")
				return enc.Encode(rows)
			case "yaml":
				enc := yaml.NewEncoder(out)
				enc.SetIndent(2)
				return enc.Encode(rows)
			default:
				w := tabwriter.NewWriter(out, 0, 0, 2, ' ', 0)
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

			out := cmd.OutOrStdout()
			switch output {
			case "yaml":
				fmt.Fprint(out, det.ManifestYAML)
				return nil
			case "json":
				enc := json.NewEncoder(out)
				enc.SetIndent("", "  ")
				return enc.Encode(det)
			default:
				printDeploymentSummary(out, det)
				return nil
			}
		},
	}
	cmd.Flags().StringVarP(&output, "output", "o", "", "output format: yaml, json (default: human summary)")
	cmd.Flags().StringVar(&stateRoot, "state-root", ".openporch",
		"directory under which openporch metadata lives")
	return cmd
}

func printDeploymentSummary(out io.Writer, det *db.DeploymentDetail) {
	fmt.Fprintf(out, "ID:         %s\n", det.ID)
	fmt.Fprintf(out, "Project:    %s\n", det.Project)
	fmt.Fprintf(out, "Env:        %s\n", det.Env)
	fmt.Fprintf(out, "EnvType:    %s\n", det.EnvType)
	fmt.Fprintf(out, "Status:     %s\n", det.Status)
	fmt.Fprintf(out, "Mode:       %s\n", det.Mode)
	fmt.Fprintf(out, "StartedAt:  %s\n", det.StartedAt)
	if det.FinishedAt != "" {
		fmt.Fprintf(out, "FinishedAt: %s\n", det.FinishedAt)
	}
	if len(det.Resources) > 0 {
		fmt.Fprintln(out, "\nResources:")
		w := tabwriter.NewWriter(out, 0, 0, 2, ' ', 0)
		fmt.Fprintln(w, "  KEY\tTYPE\tSTATUS\tRUNNER\tLOG")
		for _, res := range det.Resources {
			fmt.Fprintf(w, "  %s\t%s\t%s\t%s\t%s\n",
				res.ResourceKey, res.Type, res.Status, res.RunnerID, res.LogPath)
		}
		w.Flush()
	}
}
