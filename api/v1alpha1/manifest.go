package v1alpha1

// Manifest is the developer-facing input. Workloads are resources too; the
// runtime shape (container, Lambda, etc.) is decided by the module that the
// rule engine resolves to for a given (project, env, env_type).
type Manifest struct {
	APIVersion string                    `yaml:"apiVersion"`
	Kind       string                    `yaml:"kind"`
	Metadata   ManifestMetadata          `yaml:"metadata"`
	Workloads  map[string]Workload       `yaml:"workloads,omitempty"`
	Shared     map[string]ResourceRef    `yaml:"shared,omitempty"`
	Outputs    map[string]string         `yaml:"outputs,omitempty"`
}

type ManifestMetadata struct {
	Project     string            `yaml:"project,omitempty"`
	Name        string            `yaml:"name"`
	Annotations map[string]string `yaml:"annotations,omitempty"`
}

// Workload is one user-runnable unit. Defaults to resource type "workload".
type Workload struct {
	Type      string                 `yaml:"type,omitempty"`
	Class     string                 `yaml:"class,omitempty"`
	Params    map[string]any         `yaml:"params,omitempty"`
	Resources map[string]ResourceRef `yaml:"resources,omitempty"`
}

// ResourceRef is a manifest-level reference to a resource (shared or
// workload-scoped). The ID resolution rules:
//   - workload                : <workload-name>
//   - workload-scoped resource: workloads.<workload>.<name>
//   - shared resource         : shared.<name>
// An explicit ID overrides the default.
type ResourceRef struct {
	Type   string         `yaml:"type"`
	Class  string         `yaml:"class,omitempty"`
	ID     string         `yaml:"id,omitempty"`
	Params map[string]any `yaml:"params,omitempty"`
}
