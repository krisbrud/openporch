// Package manifest parses the developer-facing application manifest.
package manifest

import (
	"fmt"
	"os"

	"gopkg.in/yaml.v3"

	v1 "github.com/krbrudeli/openporch/api/v1alpha1"
)

// Load reads and validates a manifest file.
func Load(path string) (*v1.Manifest, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("manifest: read %s: %w", path, err)
	}
	var m v1.Manifest
	if err := yaml.Unmarshal(b, &m); err != nil {
		return nil, fmt.Errorf("manifest: parse %s: %w", path, err)
	}
	if err := Validate(&m); err != nil {
		return nil, fmt.Errorf("manifest: validate %s: %w", path, err)
	}
	return &m, nil
}

// Validate runs structural checks. JSON-Schema-level validation of params
// against the selected module's schema happens later in the pipeline.
func Validate(m *v1.Manifest) error {
	if m.APIVersion != v1.APIVersion {
		return fmt.Errorf("apiVersion must be %q, got %q", v1.APIVersion, m.APIVersion)
	}
	if m.Kind != v1.KindApplication {
		return fmt.Errorf("kind must be %q, got %q", v1.KindApplication, m.Kind)
	}
	if m.Metadata.Name == "" {
		return fmt.Errorf("metadata.name is required")
	}
	for wname, w := range m.Workloads {
		if wname == "" {
			return fmt.Errorf("workload name cannot be empty")
		}
		for rname, r := range w.Resources {
			if r.Type == "" {
				return fmt.Errorf("workloads.%s.resources.%s.type is required", wname, rname)
			}
		}
	}
	for sname, s := range m.Shared {
		if s.Type == "" {
			return fmt.Errorf("shared.%s.type is required", sname)
		}
	}
	return nil
}
