package db

import (
	"context"
	"database/sql"
	"fmt"
	"time"

	"github.com/krbrudeli/openporch/internal/deploy"
)

// SQLiteRecorder implements deploy.Recorder against a SQLite database.
type SQLiteRecorder struct {
	db *DB
}

// NewRecorder returns a Recorder backed by the given DB.
func NewRecorder(db *DB) *SQLiteRecorder {
	return &SQLiteRecorder{db: db}
}

// StartDeployment writes the deployments row and the manifest+graph
// snapshots in a single transaction.
func (r *SQLiteRecorder) StartDeployment(ctx context.Context, d deploy.DeploymentRecord) error {
	tx, err := r.db.BeginTx(ctx, nil)
	if err != nil {
		return fmt.Errorf("db: start deployment: begin: %w", err)
	}
	if _, err := tx.ExecContext(ctx,
		`INSERT INTO deployments (id, project, env, env_type, status, started_at, finished_at, mode)
		 VALUES (?, ?, ?, ?, ?, ?, NULL, ?)`,
		d.ID, d.Project, d.Env, d.EnvType, "running",
		d.StartedAt.UTC().Format(time.RFC3339), d.Mode); err != nil {
		tx.Rollback()
		return fmt.Errorf("db: insert deployment: %w", err)
	}
	if _, err := tx.ExecContext(ctx,
		`INSERT INTO deployment_manifest (deployment_id, manifest_yaml) VALUES (?, ?)`,
		d.ID, d.ManifestYAML); err != nil {
		tx.Rollback()
		return fmt.Errorf("db: insert manifest: %w", err)
	}
	if _, err := tx.ExecContext(ctx,
		`INSERT INTO deployment_graph (deployment_id, graph_json) VALUES (?, ?)`,
		d.ID, d.GraphJSON); err != nil {
		tx.Rollback()
		return fmt.Errorf("db: insert graph: %w", err)
	}
	if err := tx.Commit(); err != nil {
		return fmt.Errorf("db: start deployment: commit: %w", err)
	}
	return nil
}

// RecordResource upserts a resource row. Implemented as UPDATE-then-INSERT
// rather than ON CONFLICT / INSERT OR REPLACE so the SQL stays portable.
func (r *SQLiteRecorder) RecordResource(ctx context.Context, deploymentID string, rec deploy.ResourceRecord) error {
	var outputs sql.NullString
	if rec.OutputsJSON != "" {
		outputs = sql.NullString{String: rec.OutputsJSON, Valid: true}
	}
	res, err := r.db.ExecContext(ctx,
		`UPDATE deployment_resources
		 SET type = ?, class = ?, id = ?, module_id = ?, runner_id = ?,
		     status = ?, outputs_json = ?, log_path = ?
		 WHERE deployment_id = ? AND resource_key = ?`,
		rec.Type, rec.Class, rec.ID, rec.ModuleID, rec.RunnerID,
		rec.Status, outputs, rec.LogPath,
		deploymentID, rec.ResourceKey)
	if err != nil {
		return fmt.Errorf("db: update resource: %w", err)
	}
	n, err := res.RowsAffected()
	if err != nil {
		return fmt.Errorf("db: rows affected: %w", err)
	}
	if n > 0 {
		return nil
	}
	if _, err := r.db.ExecContext(ctx,
		`INSERT INTO deployment_resources
		 (deployment_id, resource_key, type, class, id, module_id, runner_id, status, outputs_json, log_path)
		 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
		deploymentID, rec.ResourceKey, rec.Type, rec.Class, rec.ID,
		rec.ModuleID, rec.RunnerID, rec.Status, outputs, rec.LogPath); err != nil {
		return fmt.Errorf("db: insert resource: %w", err)
	}
	return nil
}

// FinishDeployment stamps the terminal status and finished_at.
func (r *SQLiteRecorder) FinishDeployment(ctx context.Context, deploymentID string, status string, finishedAt time.Time) error {
	if _, err := r.db.ExecContext(ctx,
		`UPDATE deployments SET status = ?, finished_at = ? WHERE id = ?`,
		status, finishedAt.UTC().Format(time.RFC3339), deploymentID); err != nil {
		return fmt.Errorf("db: finish deployment: %w", err)
	}
	return nil
}
