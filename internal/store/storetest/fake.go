// Package storetest provides an in-memory fake implementation of store.Store
// for use in unit tests.
package storetest

import (
	"path/filepath"
	"sync"
)

// FileWrite records one WriteRootTF or WriteInlineModule call.
type FileWrite struct {
	Project  string
	Env      string
	Key      string
	Filename string // "main.tf" or "module/main.tf"
	Content  string
}

// Fake is an in-memory store.Store. It records writes so tests can assert on
// them without touching the filesystem. ResourceDir and LogFile return
// predictable paths under "/fake/" so runner fakes can be keyed against them.
//
// The zero value is ready to use.
type Fake struct {
	mu      sync.Mutex
	outputs map[string]map[string]any // "<project>/<env>/<key>" -> outputs

	// Writes records every WriteRootTF and WriteInlineModule call in order.
	Writes []FileWrite

	// Reads records every LoadOutputs call (project+"/"+env+"/"+key).
	Reads []string
}

func (f *Fake) ResourceDir(project, env, key string) string {
	return filepath.Join("/fake", "state", project, env, safeName(key))
}

func (f *Fake) LogFile(project, env, key, deploymentID string) string {
	return filepath.Join("/fake", "logs", project, env, deploymentID, safeName(key)+".log")
}

func (f *Fake) PluginCacheDir() string {
	return "/fake/plugin-cache"
}

func (f *Fake) WriteRootTF(project, env, key, hcl string) (string, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.Writes = append(f.Writes, FileWrite{
		Project: project, Env: env, Key: key, Filename: "main.tf", Content: hcl,
	})
	return f.ResourceDir(project, env, key), nil
}

func (f *Fake) WriteInlineModule(project, env, key, src string) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.Writes = append(f.Writes, FileWrite{
		Project: project, Env: env, Key: key, Filename: "module/main.tf", Content: src,
	})
	return nil
}

func (f *Fake) SaveOutputs(project, env, key string, outputs map[string]any) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.outputs == nil {
		f.outputs = map[string]map[string]any{}
	}
	f.outputs[project+"/"+env+"/"+key] = outputs
	return nil
}

func (f *Fake) LoadOutputs(project, env, key string) (map[string]any, bool, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.Reads = append(f.Reads, project+"/"+env+"/"+key)
	if f.outputs == nil {
		return nil, false, nil
	}
	out, ok := f.outputs[project+"/"+env+"/"+key]
	return out, ok, nil
}

func (f *Fake) ListResourceDirs(project, env string) ([]string, error) {
	return nil, nil
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
