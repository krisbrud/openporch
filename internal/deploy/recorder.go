package deploy

import (
	"context"
	"time"
)

// Recorder persists deployment history. The deploy package owns this
// interface so the database implementation can live elsewhere; swapping
// SQLite for Postgres later means swapping the implementation, not the
// callers.
//
// All methods receive timestamps and identifiers from the caller — the
// implementation must not generate them. This keeps the contract portable
// across SQL dialects.
type Recorder interface {
	StartDeployment(ctx context.Context, d DeploymentRecord) error
	RecordResource(ctx context.Context, deploymentID string, r ResourceRecord) error
	FinishDeployment(ctx context.Context, deploymentID string, status string, finishedAt time.Time) error
}

// DeploymentRecord is the snapshot written when a deployment starts.
type DeploymentRecord struct {
	ID           string
	Project      string
	Env          string
	EnvType      string
	Mode         string // "deploy" or "destroy"
	StartedAt    time.Time
	ManifestYAML string
	GraphJSON    string
}

// ResourceRecord is the snapshot of one resource's state within a
// deployment. RecordResource is upsert-style: calling it repeatedly for the
// same (deploymentID, ResourceKey) overwrites earlier rows.
type ResourceRecord struct {
	ResourceKey string
	Type        string
	Class       string
	ID          string
	ModuleID    string
	RunnerID    string
	Status      string
	OutputsJSON string
	LogPath     string
	PlanPath    string
}
