package main

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"text/tabwriter"

	"github.com/spf13/cobra"
	"gopkg.in/yaml.v3"

	"github.com/krbrudeli/openporch/internal/config"
	"github.com/krbrudeli/openporch/internal/deploy"
	"github.com/krbrudeli/openporch/internal/graph"
	"github.com/krbrudeli/openporch/internal/store/db"
)

func newGetCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "get",
		Short: "Read deployment history",
	}
	cmd.AddCommand(newGetDeploymentsCmd(), newGetDeploymentCmd(), newGetTFCmd())
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
			case "table", "":
				w := tabwriter.NewWriter(out, 0, 0, 2, ' ', 0)
				fmt.Fprintln(w, "ID\tPROJECT\tENV\tSTATUS\tSTARTED_AT\tMODE")
				for _, row := range rows {
					fmt.Fprintf(w, "%s\t%s\t%s\t%s\t%s\t%s\n",
						row.ID, row.Project, row.Env, row.Status, row.StartedAt, row.Mode)
				}
				return w.Flush()
			default:
				return fmt.Errorf("unsupported output format %q (want table, yaml, or json)", output)
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
			case "", "summary":
				printDeploymentSummary(out, det)
				return nil
			default:
				return fmt.Errorf("unsupported output format %q (want yaml or json)", output)
			}
		},
	}
	cmd.Flags().StringVarP(&output, "output", "o", "", "output format: yaml, json (default: human summary)")
	cmd.Flags().StringVar(&stateRoot, "state-root", ".openporch",
		"directory under which openporch metadata lives")
	return cmd
}

func newGetTFCmd() *cobra.Command {
	var (
		platformDir string
		outDir      string
		stateRoot   string
	)
	cmd := &cobra.Command{
		Use:   "tf <deployment-id>",
		Short: "Render saved deployment Terraform/OpenTofu HCL",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx := context.Background()
			d, err := db.Open(stateRoot)
			if err != nil {
				return err
			}
			defer d.Close()

			deploymentID := args[0]
			det, err := db.NewReader(d).GetDeployment(ctx, deploymentID)
			if err != nil {
				return err
			}
			if det == nil {
				return fmt.Errorf("deployment %q not found", deploymentID)
			}

			cfg, err := config.Load(platformDir)
			if err != nil {
				return err
			}
			g, err := deploy.GraphFromSnapshot(det.GraphJSON)
			if err != nil {
				return err
			}
			if err := hydrateGraphOutputs(g, det.Resources); err != nil {
				return err
			}

			rendered, err := deploy.RenderGraph(deploy.RenderOptions{
				Platform: cfg, Graph: g,
				ProjectID: det.Project, EnvID: det.Env, EnvTypeID: det.EnvType,
				AllowUnresolvedInputs: det.Mode == "plan_only" || det.Status != "succeeded",
			})
			if err != nil {
				return err
			}
			if outDir != "" {
				return writeRenderedTF(outDir, rendered)
			}
			printRenderedTF(cmd.OutOrStdout(), rendered)
			return nil
		},
	}
	cwd, _ := os.Getwd()
	cmd.Flags().StringVar(&platformDir, "platform", filepath.Join(cwd, "platform"),
		"directory holding platform config (resource types, modules, rules, providers)")
	cmd.Flags().StringVar(&outDir, "out", "",
		"write rendered files under this directory instead of stdout")
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

func hydrateGraphOutputs(g *graph.Graph, rows []db.ResourceRow) error {
	for _, row := range rows {
		if row.OutputsJSON == "" {
			continue
		}
		n := g.Nodes[row.ResourceKey]
		if n == nil {
			return fmt.Errorf("deployment resource %q has outputs but is missing from graph snapshot", row.ResourceKey)
		}
		var outputs map[string]any
		if err := json.Unmarshal([]byte(row.OutputsJSON), &outputs); err != nil {
			return fmt.Errorf("deployment resource %q outputs are invalid JSON: %w", row.ResourceKey, err)
		}
		n.Outputs = outputs
	}
	return nil
}

func printRenderedTF(out io.Writer, rendered []deploy.RenderedResource) {
	for i, res := range rendered {
		if i > 0 {
			fmt.Fprintln(out)
		}
		fmt.Fprintf(out, "===== %s/main.tf =====\n", res.Key)
		fmt.Fprint(out, res.HCL)
		if !strings.HasSuffix(res.HCL, "\n") {
			fmt.Fprintln(out)
		}
	}
}

func writeRenderedTF(root string, rendered []deploy.RenderedResource) error {
	for _, res := range rendered {
		dir := filepath.Join(root, tfResourceDirName(res.Key))
		if err := os.MkdirAll(dir, 0o755); err != nil {
			return fmt.Errorf("get tf: mkdir %s: %w", dir, err)
		}
		if err := os.WriteFile(filepath.Join(dir, "main.tf"), []byte(res.HCL), 0o644); err != nil {
			return fmt.Errorf("get tf: write HCL for %s: %w", res.Key, err)
		}
		if res.InlineModuleSource != "" {
			modDir := filepath.Join(dir, "module")
			if err := os.MkdirAll(modDir, 0o755); err != nil {
				return fmt.Errorf("get tf: mkdir module for %s: %w", res.Key, err)
			}
			if err := os.WriteFile(filepath.Join(modDir, "main.tf"), []byte(res.InlineModuleSource), 0o644); err != nil {
				return fmt.Errorf("get tf: write inline module for %s: %w", res.Key, err)
			}
		}
	}
	return nil
}

func tfResourceDirName(key string) string {
	out := strings.Map(func(r rune) rune {
		switch r {
		case '/', '\\':
			return '_'
		default:
			return r
		}
	}, key)
	if out == "" {
		return "_"
	}
	return out
}
