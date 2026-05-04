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
	// SetActiveResources atomically replaces the active resource set for
	// (project, env) with the provided slice, tagging each row with
	// deploymentID. Resources previously active but absent from the new
	// slice are removed.
	SetActiveResources(ctx context.Context, project, env, deploymentID string, resources []ActiveResourceRecord) error
	// ClearActiveResources removes all active resources for (project, env).
	ClearActiveResources(ctx context.Context, project, env string) error
}

// ActiveResourceRecord captures the identity and outputs of one resource
// in the live set for an environment.
type ActiveResourceRecord struct {
	ResourceKey string
	Type        string
	Class       string
	ID          string
	ModuleID    string
	OutputsJSON string
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
