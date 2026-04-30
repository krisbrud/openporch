// Package store persists deployment outcomes between runs. v0 uses
// per-resource JSON files inside each resource's working directory; SQLite
// arrives in v0.3 along with deployment history.
package store

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"
)

// FS is a filesystem-backed store rooted at .openporch/.
type FS struct {
	Root string // e.g. ".openporch"
}

// ResourceDir returns the per-resource working directory.
// Layout: <Root>/state/<project>/<env>/<resourceDirName>/
//
// resourceDirName is derived from the canonical graph key by replacing the
// pipe separators with double dashes so it's filesystem-safe.
func (s *FS) ResourceDir(project, env, key string) string {
	safe := safeName(key)
	return filepath.Join(s.Root, "state", project, env, safe)
}

// LogFile returns the log path for a resource within a deployment.
func (s *FS) LogFile(project, env, key, deploymentID string) string {
	return filepath.Join(s.Root, "logs", project, env, deploymentID, safeName(key)+".log")
}

// PluginCacheDir returns the shared TF_PLUGIN_CACHE_DIR.
func (s *FS) PluginCacheDir() string {
	return filepath.Join(s.Root, "plugin-cache")
}

// SaveOutputs writes the resource's outputs.json next to its TF state.
func (s *FS) SaveOutputs(project, env, key string, outputs map[string]any) error {
	dir := s.ResourceDir(project, env, key)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return fmt.Errorf("store: mkdir %s: %w", dir, err)
	}
	b, err := json.MarshalIndent(outputs, "", "  ")
	if err != nil {
		return fmt.Errorf("store: marshal outputs: %w", err)
	}
	if err := os.WriteFile(filepath.Join(dir, "outputs.json"), b, 0o644); err != nil {
		return fmt.Errorf("store: write outputs: %w", err)
	}
	return nil
}

// LoadOutputs reads the resource's outputs.json. Returns false if missing.
func (s *FS) LoadOutputs(project, env, key string) (map[string]any, bool, error) {
	p := filepath.Join(s.ResourceDir(project, env, key), "outputs.json")
	b, err := os.ReadFile(p)
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return nil, false, nil
		}
		return nil, false, fmt.Errorf("store: read %s: %w", p, err)
	}
	var out map[string]any
	if err := json.Unmarshal(b, &out); err != nil {
		return nil, false, fmt.Errorf("store: parse %s: %w", p, err)
	}
	return out, true, nil
}

// WriteRootTF writes main.tf into the resource's working directory.
func (s *FS) WriteRootTF(project, env, key, hcl string) (string, error) {
	dir := s.ResourceDir(project, env, key)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return "", fmt.Errorf("store: mkdir %s: %w", dir, err)
	}
	p := filepath.Join(dir, "main.tf")
	if err := os.WriteFile(p, []byte(hcl), 0o644); err != nil {
		return "", fmt.Errorf("store: write %s: %w", p, err)
	}
	return dir, nil
}

// WriteInlineModule writes inline TF source under <resourceDir>/module/main.tf.
func (s *FS) WriteInlineModule(project, env, key, src string) error {
	dir := filepath.Join(s.ResourceDir(project, env, key), "module")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return fmt.Errorf("store: mkdir %s: %w", dir, err)
	}
	p := filepath.Join(dir, "main.tf")
	if err := os.WriteFile(p, []byte(src), 0o644); err != nil {
		return fmt.Errorf("store: write %s: %w", p, err)
	}
	return nil
}

func safeName(s string) string {
	out := make([]byte, 0, len(s))
	for i := 0; i < len(s); i++ {
		c := s[i]
		switch {
		case c >= 'A' && c <= 'Z', c >= 'a' && c <= 'z', c >= '0' && c <= '9', c == '-', c == '_', c == '.':
			out = append(out, c)
		default:
			out = append(out, '_')
		}
	}
	return string(out)
}

// ListResourceDirs returns the set of resource directories present under a
// given project/env, useful for cleanup or destroy paths in later versions.
func (s *FS) ListResourceDirs(project, env string) ([]string, error) {
	root := filepath.Join(s.Root, "state", project, env)
	entries, err := os.ReadDir(root)
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return nil, nil
		}
		return nil, err
	}
	var out []string
	for _, e := range entries {
		if e.IsDir() {
			out = append(out, filepath.Join(root, e.Name()))
		}
	}
	sort.Strings(out)
	return out, nil
}
