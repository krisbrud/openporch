package db

import (
	"context"
	"database/sql"
	"fmt"
)

// DeploymentRow is a single row from the deployments table.
type DeploymentRow struct {
	ID         string `json:"id" yaml:"id"`
	Project    string `json:"project" yaml:"project"`
	Env        string `json:"env" yaml:"env"`
	EnvType    string `json:"env_type" yaml:"env_type"`
	Status     string `json:"status" yaml:"status"`
	StartedAt  string `json:"started_at" yaml:"started_at"`
	FinishedAt string `json:"finished_at,omitempty" yaml:"finished_at,omitempty"`
	Mode       string `json:"mode" yaml:"mode"`
}

// ResourceRow is a single row from the deployment_resources table.
type ResourceRow struct {
	ResourceKey string `json:"resource_key" yaml:"resource_key"`
	Type        string `json:"type" yaml:"type"`
	Class       string `json:"class" yaml:"class"`
	ID          string `json:"id" yaml:"id"`
	ModuleID    string `json:"module_id" yaml:"module_id"`
	RunnerID    string `json:"runner_id" yaml:"runner_id"`
	Status      string `json:"status" yaml:"status"`
	OutputsJSON string `json:"outputs_json,omitempty" yaml:"outputs_json,omitempty"`
	LogPath     string `json:"log_path" yaml:"log_path"`
}

// DeploymentDetail is a full deployment record including manifest and resources.
type DeploymentDetail struct {
	ID           string        `json:"id" yaml:"id"`
	Project      string        `json:"project" yaml:"project"`
	Env          string        `json:"env" yaml:"env"`
	EnvType      string        `json:"env_type" yaml:"env_type"`
	Status       string        `json:"status" yaml:"status"`
	StartedAt    string        `json:"started_at" yaml:"started_at"`
	FinishedAt   string        `json:"finished_at,omitempty" yaml:"finished_at,omitempty"`
	Mode         string        `json:"mode" yaml:"mode"`
	ManifestYAML string        `json:"manifest_yaml" yaml:"manifest_yaml"`
	Resources    []ResourceRow `json:"resources" yaml:"resources"`
}

// Reader queries the deployment history stored in DB.
type Reader struct {
	db *DB
}

// NewReader returns a Reader backed by the given DB.
func NewReader(db *DB) *Reader {
	return &Reader{db: db}
}

// ListDeployments returns deployments filtered by project and env (empty = all),
// ordered by started_at descending, capped at limit rows.
func (r *Reader) ListDeployments(ctx context.Context, project, env string, limit int) ([]DeploymentRow, error) {
	rows, err := r.db.QueryContext(ctx,
		`SELECT id, project, env, env_type, status, started_at, COALESCE(finished_at, ''), mode
		 FROM deployments
		 WHERE (? = '' OR project = ?) AND (? = '' OR env = ?)
		 ORDER BY started_at DESC
		 LIMIT ?`,
		project, project, env, env, limit)
	if err != nil {
		return nil, fmt.Errorf("db: list deployments: %w", err)
	}
	defer rows.Close()
	var out []DeploymentRow
	for rows.Next() {
		var d DeploymentRow
		if err := rows.Scan(&d.ID, &d.Project, &d.Env, &d.EnvType, &d.Status, &d.StartedAt, &d.FinishedAt, &d.Mode); err != nil {
			return nil, fmt.Errorf("db: scan deployment row: %w", err)
		}
		out = append(out, d)
	}
	if err := rows.Err(); err != nil {
		return nil, fmt.Errorf("db: list deployments: %w", err)
	}
	if out == nil {
		out = []DeploymentRow{}
	}
	return out, nil
}

// GetDeployment returns the full record for a single deployment, or nil if not found.
func (r *Reader) GetDeployment(ctx context.Context, id string) (*DeploymentDetail, error) {
	row := r.db.QueryRowContext(ctx,
		`SELECT id, project, env, env_type, status, started_at, COALESCE(finished_at, ''), mode
		 FROM deployments WHERE id = ?`, id)
	var d DeploymentDetail
	err := row.Scan(&d.ID, &d.Project, &d.Env, &d.EnvType, &d.Status, &d.StartedAt, &d.FinishedAt, &d.Mode)
	if err == sql.ErrNoRows {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("db: get deployment: %w", err)
	}

	mRow := r.db.QueryRowContext(ctx,
		`SELECT manifest_yaml FROM deployment_manifest WHERE deployment_id = ?`, id)
	if err := mRow.Scan(&d.ManifestYAML); err != nil && err != sql.ErrNoRows {
		return nil, fmt.Errorf("db: get deployment manifest: %w", err)
	}

	resRows, err := r.db.QueryContext(ctx,
		`SELECT resource_key, type, class, id, module_id, runner_id, status,
		        COALESCE(outputs_json, ''), log_path
		 FROM deployment_resources WHERE deployment_id = ?
		 ORDER BY resource_key`, id)
	if err != nil {
		return nil, fmt.Errorf("db: get deployment resources: %w", err)
	}
	defer resRows.Close()
	for resRows.Next() {
		var res ResourceRow
		if err := resRows.Scan(&res.ResourceKey, &res.Type, &res.Class, &res.ID,
			&res.ModuleID, &res.RunnerID, &res.Status, &res.OutputsJSON, &res.LogPath); err != nil {
			return nil, fmt.Errorf("db: scan resource row: %w", err)
		}
		d.Resources = append(d.Resources, res)
	}
	if err := resRows.Err(); err != nil {
		return nil, fmt.Errorf("db: get deployment resources: %w", err)
	}
	if d.Resources == nil {
		d.Resources = []ResourceRow{}
	}
	return &d, nil
}
