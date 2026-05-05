package main

import (
	"context"
	"fmt"
	"strings"

	v1 "github.com/krbrudeli/openporch/api/v1alpha1"
	"github.com/krbrudeli/openporch/internal/manifest"
	"github.com/krbrudeli/openporch/internal/store/db"
)

// manifestSource is the parsed form of the deploy command's positional
// argument. Exactly one of File, Deployment, or Environment is set.
type manifestSource struct {
	// File is a filesystem path to a YAML manifest (no scheme).
	File string
	// Deployment is set when the source is `deployment://<HEAD|UUID>`.
	// Head reports whether the special `HEAD` selector was used.
	Deployment   string
	DeploymentID string
	Head         bool
	// Environment is set when the source is `environment://<env-id>`.
	Environment string
}

// parseManifestSource classifies the deploy positional argument. It does not
// touch the database or filesystem.
func parseManifestSource(arg string) (manifestSource, error) {
	switch {
	case strings.HasPrefix(arg, "deployment://"):
		ref := strings.TrimPrefix(arg, "deployment://")
		if ref == "" {
			return manifestSource{}, fmt.Errorf("deployment source requires HEAD or a deployment id: %q", arg)
		}
		s := manifestSource{Deployment: ref}
		if ref == "HEAD" {
			s.Head = true
		} else {
			s.DeploymentID = ref
		}
		return s, nil
	case strings.HasPrefix(arg, "environment://"):
		ref := strings.TrimPrefix(arg, "environment://")
		if ref == "" {
			return manifestSource{}, fmt.Errorf("environment source requires an environment id: %q", arg)
		}
		return manifestSource{Environment: ref}, nil
	case strings.Contains(arg, "://"):
		scheme := arg[:strings.Index(arg, "://")]
		return manifestSource{}, fmt.Errorf("unknown manifest source scheme %q (want deployment:// or environment://)", scheme)
	default:
		if arg == "" {
			return manifestSource{}, fmt.Errorf("manifest source is required")
		}
		return manifestSource{File: arg}, nil
	}
}

// resolveManifestSource turns a parsed source into a *v1.Manifest. For
// non-file sources it queries the SQLite reader for the deployment whose
// stored manifest_yaml should be replayed; project and env qualify the query
// for HEAD and environment lookups (and are ignored for explicit IDs).
func resolveManifestSource(ctx context.Context, rdr *db.Reader, src manifestSource, project, env string) (*v1.Manifest, error) {
	switch {
	case src.File != "":
		return manifest.Load(src.File)
	case src.Head:
		if project == "" || env == "" {
			return nil, fmt.Errorf("deployment://HEAD requires --project and --env")
		}
		det, err := rdr.GetLastSuccessfulDeployment(ctx, project, env)
		if err != nil {
			return nil, err
		}
		if det == nil {
			return nil, fmt.Errorf("no successful deployment found for project=%q env=%q", project, env)
		}
		return parseStoredManifest(det.ManifestYAML, det.ID)
	case src.DeploymentID != "":
		det, err := rdr.GetDeployment(ctx, src.DeploymentID)
		if err != nil {
			return nil, err
		}
		if det == nil {
			return nil, fmt.Errorf("deployment %q not found", src.DeploymentID)
		}
		return parseStoredManifest(det.ManifestYAML, det.ID)
	case src.Environment != "":
		if project == "" {
			return nil, fmt.Errorf("environment://%s requires --project", src.Environment)
		}
		det, err := rdr.GetLastSuccessfulDeployment(ctx, project, src.Environment)
		if err != nil {
			return nil, err
		}
		if det == nil {
			return nil, fmt.Errorf("no successful deployment found for project=%q env=%q", project, src.Environment)
		}
		return parseStoredManifest(det.ManifestYAML, det.ID)
	default:
		return nil, fmt.Errorf("manifest source is required")
	}
}

func parseStoredManifest(yamlText, deploymentID string) (*v1.Manifest, error) {
	if yamlText == "" {
		return nil, fmt.Errorf("deployment %q has no stored manifest", deploymentID)
	}
	m, err := manifest.LoadBytes([]byte(yamlText))
	if err != nil {
		return nil, fmt.Errorf("deployment %q: %w", deploymentID, err)
	}
	return m, nil
}
