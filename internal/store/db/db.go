// Package db is the SQLite-backed store for deployment history. It is the
// only place in the codebase aware of any SQL dialect; everything else uses
// the deploy.Recorder interface so a Postgres implementation can drop in
// later by satisfying the same contract.
package db

import (
	"context"
	"database/sql"
	"fmt"
	"os"
	"path/filepath"
	"time"

	_ "modernc.org/sqlite"
)

// DB wraps an *sql.DB connection to the openporch metadata store.
type DB struct {
	*sql.DB
}

// Open returns a DB rooted at <stateRoot>/openporch.db, creating the file
// and applying any outstanding migrations. The state root directory is
// created if missing.
func Open(stateRoot string) (*DB, error) {
	if err := os.MkdirAll(stateRoot, 0o755); err != nil {
		return nil, fmt.Errorf("db: mkdir %s: %w", stateRoot, err)
	}
	path := filepath.Join(stateRoot, "openporch.db")
	sqlDB, err := sql.Open("sqlite", path)
	if err != nil {
		return nil, fmt.Errorf("db: open %s: %w", path, err)
	}
	// SQLite-specific knobs. WAL gives us readers-during-writers, the
	// busy_timeout avoids spurious "database is locked" under contention,
	// and foreign_keys lets the FKs in our schema actually fire.
	pragmas := []string{
		"PRAGMA journal_mode=WAL",
		"PRAGMA foreign_keys=ON",
		"PRAGMA busy_timeout=5000",
	}
	for _, p := range pragmas {
		if _, err := sqlDB.Exec(p); err != nil {
			sqlDB.Close()
			return nil, fmt.Errorf("db: %s: %w", p, err)
		}
	}
	if err := migrate(sqlDB); err != nil {
		sqlDB.Close()
		return nil, err
	}
	return &DB{sqlDB}, nil
}

// migrations hold ANSI-standard SQL DDL — no SQLite-only constructs. New
// migrations append to the slice; their index+1 is their version number.
var migrations = []string{
	`CREATE TABLE deployments (
		id          TEXT PRIMARY KEY,
		project     TEXT NOT NULL,
		env         TEXT NOT NULL,
		env_type    TEXT NOT NULL,
		status      TEXT NOT NULL,
		started_at  TEXT NOT NULL,
		finished_at TEXT,
		mode        TEXT NOT NULL
	);
	CREATE TABLE deployment_resources (
		deployment_id TEXT NOT NULL REFERENCES deployments(id),
		resource_key  TEXT NOT NULL,
		type          TEXT NOT NULL,
		class         TEXT NOT NULL,
		id            TEXT NOT NULL,
		module_id     TEXT NOT NULL,
		runner_id     TEXT NOT NULL,
		status        TEXT NOT NULL,
		outputs_json  TEXT,
		log_path      TEXT NOT NULL,
		PRIMARY KEY (deployment_id, resource_key)
	);
	CREATE TABLE deployment_manifest (
		deployment_id TEXT PRIMARY KEY REFERENCES deployments(id),
		manifest_yaml TEXT NOT NULL
	);
	CREATE TABLE deployment_graph (
		deployment_id TEXT PRIMARY KEY REFERENCES deployments(id),
		graph_json    TEXT NOT NULL
	);`,
}

func migrate(db *sql.DB) error {
	ctx := context.Background()
	if _, err := db.ExecContext(ctx, `CREATE TABLE IF NOT EXISTS _schema_version (
		version    INTEGER NOT NULL,
		applied_at TEXT    NOT NULL
	)`); err != nil {
		return fmt.Errorf("db: bootstrap _schema_version: %w", err)
	}
	current, err := currentVersion(ctx, db)
	if err != nil {
		return err
	}
	for i := current; i < len(migrations); i++ {
		version := i + 1
		tx, err := db.BeginTx(ctx, nil)
		if err != nil {
			return fmt.Errorf("db: begin migration %d: %w", version, err)
		}
		if _, err := tx.ExecContext(ctx, migrations[i]); err != nil {
			tx.Rollback()
			return fmt.Errorf("db: apply migration %d: %w", version, err)
		}
		if _, err := tx.ExecContext(ctx,
			`INSERT INTO _schema_version (version, applied_at) VALUES (?, ?)`,
			version, time.Now().UTC().Format(time.RFC3339)); err != nil {
			tx.Rollback()
			return fmt.Errorf("db: record migration %d: %w", version, err)
		}
		if err := tx.Commit(); err != nil {
			return fmt.Errorf("db: commit migration %d: %w", version, err)
		}
	}
	return nil
}

func currentVersion(ctx context.Context, db *sql.DB) (int, error) {
	var v sql.NullInt64
	row := db.QueryRowContext(ctx, `SELECT MAX(version) FROM _schema_version`)
	if err := row.Scan(&v); err != nil {
		return 0, fmt.Errorf("db: read schema version: %w", err)
	}
	if !v.Valid {
		return 0, nil
	}
	return int(v.Int64), nil
}
