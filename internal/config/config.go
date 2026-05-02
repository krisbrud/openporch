// Package config loads platform-engineer-authored configuration: resource
// types, modules, module rules, providers, runners, and runner rules. Files
// can live anywhere under the platform directory, in any layout. Each YAML
// document must declare its kind via apiVersion+kind.
package config

import (
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"strings"

	"gopkg.in/yaml.v3"

	v1 "github.com/krbrudeli/openporch/api/v1alpha1"
)

// Load reads every .yaml/.yml file under root, splits on YAML document
// boundaries, and dispatches each document by kind into the platform config.
func Load(root string) (*v1.PlatformConfig, error) {
	absRoot, err := filepath.Abs(root)
	if err != nil {
		return nil, fmt.Errorf("config: resolve root %s: %w", root, err)
	}
	cfg := &v1.PlatformConfig{
		RootDir:       absRoot,
		ResourceTypes: map[string]v1.ResourceType{},
		Modules:       map[string]v1.Module{},
		Providers:     map[string]v1.Provider{},
		Runners:       map[string]v1.Runner{},
	}
	err = filepath.WalkDir(root, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if d.IsDir() {
			return nil
		}
		ext := strings.ToLower(filepath.Ext(path))
		if ext != ".yaml" && ext != ".yml" {
			return nil
		}
		return loadFile(path, cfg)
	})
	if err != nil {
		return nil, fmt.Errorf("config: walk %s: %w", root, err)
	}
	if err := Validate(cfg); err != nil {
		return nil, err
	}
	return cfg, nil
}

func loadFile(path string, cfg *v1.PlatformConfig) error {
	b, err := os.ReadFile(path)
	if err != nil {
		return fmt.Errorf("config: read %s: %w", path, err)
	}
	dec := yaml.NewDecoder(strings.NewReader(string(b)))
	docIdx := 0
	for {
		var node yaml.Node
		if err := dec.Decode(&node); err != nil {
			if errors.Is(err, io.EOF) {
				return nil
			}
			return fmt.Errorf("config: decode %s [doc %d]: %w", path, docIdx, err)
		}
		if node.Kind == 0 {
			docIdx++
			continue
		}
		var meta struct {
			Kind string `yaml:"kind"`
		}
		if err := node.Decode(&meta); err != nil {
			return fmt.Errorf("config: read kind %s [doc %d]: %w", path, docIdx, err)
		}
		if err := dispatch(path, docIdx, &node, meta.Kind, cfg); err != nil {
			return err
		}
		docIdx++
	}
}

func dispatch(path string, docIdx int, node *yaml.Node, kind string, cfg *v1.PlatformConfig) error {
	switch kind {
	case v1.KindResourceType:
		var rt v1.ResourceType
		if err := node.Decode(&rt); err != nil {
			return decodeErr(path, docIdx, kind, err)
		}
		if rt.ID == "" {
			return fmt.Errorf("config: %s [doc %d]: ResourceType.id is required", path, docIdx)
		}
		cfg.ResourceTypes[rt.ID] = rt
	case v1.KindModule:
		var m v1.Module
		if err := node.Decode(&m); err != nil {
			return decodeErr(path, docIdx, kind, err)
		}
		if m.ID == "" {
			return fmt.Errorf("config: %s [doc %d]: Module.id is required", path, docIdx)
		}
		cfg.Modules[m.ID] = m
	case v1.KindModuleRule:
		var r v1.ModuleRule
		if err := node.Decode(&r); err != nil {
			return decodeErr(path, docIdx, kind, err)
		}
		if r.ID == "" {
			return fmt.Errorf("config: %s [doc %d]: ModuleRule.id is required", path, docIdx)
		}
		cfg.ModuleRules = append(cfg.ModuleRules, r)
	case v1.KindProvider:
		var p v1.Provider
		if err := node.Decode(&p); err != nil {
			return decodeErr(path, docIdx, kind, err)
		}
		if p.ID == "" {
			return fmt.Errorf("config: %s [doc %d]: Provider.id is required", path, docIdx)
		}
		cfg.Providers[p.ID] = p
	case v1.KindRunner:
		var r v1.Runner
		if err := node.Decode(&r); err != nil {
			return decodeErr(path, docIdx, kind, err)
		}
		if r.ID == "" {
			return fmt.Errorf("config: %s [doc %d]: Runner.id is required", path, docIdx)
		}
		cfg.Runners[r.ID] = r
	case v1.KindRunnerRule:
		var r v1.RunnerRule
		if err := node.Decode(&r); err != nil {
			return decodeErr(path, docIdx, kind, err)
		}
		if r.ID == "" {
			return fmt.Errorf("config: %s [doc %d]: RunnerRule.id is required", path, docIdx)
		}
		cfg.RunnerRules = append(cfg.RunnerRules, r)
	case "":
		// Allow empty/comment-only documents.
	default:
		return fmt.Errorf("config: %s [doc %d]: unknown kind %q", path, docIdx, kind)
	}
	return nil
}

func decodeErr(path string, docIdx int, kind string, err error) error {
	return fmt.Errorf("config: %s [doc %d] %s: %w", path, docIdx, kind, err)
}

// Validate cross-references: rules point at known modules, modules point at
// known resource types and known providers, etc.
func Validate(cfg *v1.PlatformConfig) error {
	for _, m := range cfg.Modules {
		if _, ok := cfg.ResourceTypes[m.ResourceType]; !ok {
			return fmt.Errorf("config: module %q references unknown resource_type %q",
				m.ID, m.ResourceType)
		}
		for depName, dep := range m.Dependencies {
			// A module depending on its own resource type with an inheriting
			// ID (empty or @-prefixed) regenerates a fresh node every graph
			// expansion pass — guaranteed runaway. Real coprovisioned-replica
			// use cases must spell out an explicit ID that doesn't depend on
			// the parent.
			if dep.Type == m.ResourceType && (dep.ID == "" || strings.HasPrefix(dep.ID, "@")) {
				return fmt.Errorf("config: module %q dependency %q targets its own resource_type %q with inheriting id %q — this would expand infinitely; set an explicit id if you really want a sibling resource of the same type",
					m.ID, depName, dep.Type, dep.ID)
			}
		}
		for local, ref := range m.ProviderMapping {
			// Expected form: "<provider_type>.<provider_id>"
			parts := strings.SplitN(ref, ".", 2)
			if len(parts) != 2 {
				return fmt.Errorf("config: module %q provider_mapping[%q]=%q: expected '<type>.<id>'",
					m.ID, local, ref)
			}
			pid := parts[1]
			if _, ok := cfg.Providers[pid]; !ok {
				return fmt.Errorf("config: module %q provider_mapping[%q] references unknown provider %q",
					m.ID, local, pid)
			}
			if cfg.Providers[pid].ProviderType != parts[0] {
				return fmt.Errorf("config: module %q provider_mapping[%q]: provider %q has type %q, mapping says %q",
					m.ID, local, pid, cfg.Providers[pid].ProviderType, parts[0])
			}
		}
	}
	for _, r := range cfg.ModuleRules {
		if _, ok := cfg.Modules[r.ModuleID]; !ok {
			return fmt.Errorf("config: module rule %q references unknown module %q",
				r.ID, r.ModuleID)
		}
		if _, ok := cfg.ResourceTypes[r.ResourceType]; !ok {
			return fmt.Errorf("config: module rule %q references unknown resource_type %q",
				r.ID, r.ResourceType)
		}
		if cfg.Modules[r.ModuleID].ResourceType != r.ResourceType {
			return fmt.Errorf("config: module rule %q: module %q has type %q, rule says %q",
				r.ID, r.ModuleID, cfg.Modules[r.ModuleID].ResourceType, r.ResourceType)
		}
	}
	for _, r := range cfg.RunnerRules {
		if _, ok := cfg.Runners[r.RunnerID]; !ok {
			return fmt.Errorf("config: runner rule %q references unknown runner %q",
				r.ID, r.RunnerID)
		}
	}
	if err := validateModuleHCL(cfg); err != nil {
		return err
	}
	return nil
}
