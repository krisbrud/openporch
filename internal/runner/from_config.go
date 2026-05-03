package runner

import (
	"fmt"

	v1 "github.com/krbrudeli/openporch/api/v1alpha1"
)

// FromConfig constructs a Runner from a platform Runner definition.
// binaryPath and pluginCacheDir are forwarded to LocalTofu for type "local-tofu".
func FromConfig(r v1.Runner, binaryPath, pluginCacheDir string) (Runner, error) {
	switch r.Type {
	case v1.RunnerLocalTofu:
		return &LocalTofu{
			BinaryPath:     binaryPath,
			PluginCacheDir: pluginCacheDir,
		}, nil
	default:
		return nil, fmt.Errorf("runner: unsupported type %q for runner %q", r.Type, r.ID)
	}
}
