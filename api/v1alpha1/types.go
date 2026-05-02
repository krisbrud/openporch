// Package v1alpha1 contains the public Go types for openporch's platform
// configuration and developer-facing manifest.
package v1alpha1

const (
	APIVersion       = "openporch/v1alpha1"
	KindApplication  = "Application"
	KindResourceType = "ResourceType"
	KindModule       = "Module"
	KindModuleRule   = "ModuleRule"
	KindProvider     = "Provider"
	KindRunner       = "Runner"
	KindRunnerRule   = "RunnerRule"

	DefaultClass        = "default"
	WorkloadType        = "workload"
	RunnerLocalTofu     = "local-tofu"
)

// ResourceType is the contract a module must satisfy.
type ResourceType struct {
	APIVersion            string         `yaml:"apiVersion,omitempty"`
	Kind                  string         `yaml:"kind,omitempty"`
	ID                    string         `yaml:"id"`
	OutputSchema          map[string]any `yaml:"output_schema,omitempty"`
	IsDeveloperAccessible *bool          `yaml:"is_developer_accessible,omitempty"`
	Description           string         `yaml:"description,omitempty"`
}

// Module is OpenTofu code that implements a ResourceType.
type Module struct {
	APIVersion       string                `yaml:"apiVersion,omitempty"`
	Kind             string                `yaml:"kind,omitempty"`
	ID               string                `yaml:"id"`
	ResourceType     string                `yaml:"resource_type"`
	ModuleSource     string                `yaml:"module_source"`
	ModuleSourceCode string                `yaml:"module_source_code,omitempty"`
	ModuleInputs     map[string]any        `yaml:"module_inputs,omitempty"`
	ModuleParams     map[string]any        `yaml:"module_params,omitempty"`
	ProviderMapping  map[string]string     `yaml:"provider_mapping,omitempty"`
	Dependencies     map[string]Dependency `yaml:"dependencies,omitempty"`
	Coprovisioned    []Coprovisioned       `yaml:"coprovisioned,omitempty"`
	Description      string                `yaml:"description,omitempty"`
}

// Dependency is a resource that must be provisioned before this module's resource.
type Dependency struct {
	Type   string         `yaml:"type"`
	Class  string         `yaml:"class,omitempty"`
	ID     string         `yaml:"id,omitempty"`
	Params map[string]any `yaml:"params,omitempty"`
}

// Coprovisioned is a resource provisioned alongside this module's resource.
type Coprovisioned struct {
	Type                 string         `yaml:"type"`
	Class                string         `yaml:"class,omitempty"`
	ID                   string         `yaml:"id,omitempty"`
	Params               map[string]any `yaml:"params,omitempty"`
	IsDependentOnCurrent bool           `yaml:"is_dependent_on_current,omitempty"`
}

// ModuleRule selects a module for a resource type given environment matchers.
// Weights: env_type_id=1, project_id=2, env_id=4, resource_id=8, resource_class=16.
// Highest total wins. Empty rule = catch-all.
type ModuleRule struct {
	APIVersion    string `yaml:"apiVersion,omitempty"`
	Kind          string `yaml:"kind,omitempty"`
	ID            string `yaml:"id"`
	ResourceType  string `yaml:"resource_type"`
	ModuleID      string `yaml:"module_id"`
	EnvTypeID     string `yaml:"env_type_id,omitempty"`
	ProjectID     string `yaml:"project_id,omitempty"`
	EnvID         string `yaml:"env_id,omitempty"`
	ResourceID    string `yaml:"resource_id,omitempty"`
	ResourceClass string `yaml:"resource_class,omitempty"`
}

// Provider is a central OpenTofu provider configuration injected at the root.
type Provider struct {
	APIVersion        string         `yaml:"apiVersion,omitempty"`
	Kind              string         `yaml:"kind,omitempty"`
	ID                string         `yaml:"id"`
	ProviderType      string         `yaml:"provider_type"`
	Source            string         `yaml:"source"`
	VersionConstraint string         `yaml:"version_constraint,omitempty"`
	Configuration     map[string]any `yaml:"configuration,omitempty"`
	Description       string         `yaml:"description,omitempty"`
}

// Runner is an execution backend.
type Runner struct {
	APIVersion    string         `yaml:"apiVersion,omitempty"`
	Kind          string         `yaml:"kind,omitempty"`
	ID            string         `yaml:"id"`
	Type          string         `yaml:"type"`
	Configuration map[string]any `yaml:"configuration,omitempty"`
	Description   string         `yaml:"description,omitempty"`
}

// RunnerRule maps (project_id, env_type_id) → runner_id.
// Weights: env_type_id=1, project_id=2. Highest total wins; empty = catch-all.
type RunnerRule struct {
	APIVersion string `yaml:"apiVersion,omitempty"`
	Kind       string `yaml:"kind,omitempty"`
	ID         string `yaml:"id"`
	RunnerID   string `yaml:"runner_id"`
	ProjectID  string `yaml:"project_id,omitempty"`
	EnvTypeID  string `yaml:"env_type_id,omitempty"`
}

// PlatformConfig aggregates everything loaded from ./platform/.
type PlatformConfig struct {
	// RootDir is the absolute path of the directory the config was loaded
	// from. Local module_source paths are resolved against this.
	RootDir       string
	ResourceTypes map[string]ResourceType
	Modules       map[string]Module
	ModuleRules   []ModuleRule
	Providers     map[string]Provider
	Runners       map[string]Runner
	RunnerRules   []RunnerRule
}
