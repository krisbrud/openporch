package provider

import (
	"fmt"
	"sort"
	"strings"

	"github.com/krisbrud/openporch/provider/internal/client"
)

// Secret driver inputs are nested JSON objects, such as
// `{variables = {password = "…"}}`. These helpers address their leaves by path.

// inputPath is a leaf's path, its keys joined by a separator no key contains.
type inputPath string

const pathSep = "\x00"

func (p inputPath) keys() []string { return strings.Split(string(p), pathSep) }

func (p inputPath) String() string { return strings.Join(p.keys(), ".") }

func childPath(parent inputPath, key string) inputPath {
	if parent == "" {
		return inputPath(key)
	}
	return parent + pathSep + inputPath(key)
}

// stringLeaves flattens an object whose leaves are strings.
func stringLeaves(x any) (map[inputPath]string, error) {
	out := map[inputPath]string{}
	var walk func(inputPath, any) error
	walk = func(p inputPath, x any) error {
		switch x := x.(type) {
		case nil:
			return nil
		case string:
			if p == "" {
				return fmt.Errorf("expected an object")
			}
			out[p] = x
			return nil
		case map[string]any:
			for k, v := range x {
				if err := walk(childPath(p, k), v); err != nil {
					return err
				}
			}
			return nil
		}
		return fmt.Errorf("%s must be a string or an object of strings", p)
	}
	return out, walk("", x)
}

// isRefLeaf reports whether x is a secret reference object.
func isRefLeaf(x any) bool {
	m, ok := x.(map[string]any)
	if !ok {
		return false
	}
	_, store := m["store"]
	_, ref := m["ref"]
	return store && ref
}

// refLeaves flattens an object whose leaves are secret references.
func refLeaves(x any) (map[inputPath]client.SecretRef, error) {
	out := map[inputPath]client.SecretRef{}
	var walk func(inputPath, any) error
	walk = func(p inputPath, x any) error {
		if x == nil {
			return nil
		}
		if p != "" && isRefLeaf(x) {
			m := x.(map[string]any)
			str := func(k string) (string, error) {
				switch v := m[k].(type) {
				case nil:
					return "", nil
				case string:
					return v, nil
				}
				return "", fmt.Errorf("%s.%s must be a string", p, k)
			}
			var r client.SecretRef
			var err error
			if r.Store, err = str("store"); err != nil {
				return err
			}
			if r.Ref, err = str("ref"); err != nil {
				return err
			}
			if r.Version, err = str("version"); err != nil {
				return err
			}
			for k := range m {
				if k != "store" && k != "ref" && k != "version" {
					return fmt.Errorf("%s has an unknown key %q; a reference has store, ref and version", p, k)
				}
			}
			out[p] = r
			return nil
		}
		m, ok := x.(map[string]any)
		if !ok {
			return fmt.Errorf("%s must be a reference {store, ref, version} or an object of references", p)
		}
		for k, v := range m {
			if err := walk(childPath(p, k), v); err != nil {
				return err
			}
		}
		return nil
	}
	if x != nil {
		if _, ok := x.(map[string]any); !ok {
			return nil, fmt.Errorf("expected an object")
		}
	}
	return out, walk("", x)
}

// nest builds the nested object holding each leaf at its path.
func nest[V any](leaves map[inputPath]V, leaf func(V) any) map[string]any {
	out := map[string]any{}
	paths := make([]inputPath, 0, len(leaves))
	for p := range leaves {
		paths = append(paths, p)
	}
	sort.Slice(paths, func(i, j int) bool { return paths[i] < paths[j] })
	for _, p := range paths {
		keys := p.keys()
		m := out
		for _, k := range keys[:len(keys)-1] {
			next, ok := m[k].(map[string]any)
			if !ok {
				next = map[string]any{}
				m[k] = next
			}
			m = next
		}
		m[keys[len(keys)-1]] = leaf(leaves[p])
	}
	return out
}

func refJSON(r client.SecretRef) any {
	m := map[string]any{"store": r.Store, "ref": r.Ref}
	if r.Version != "" {
		m["version"] = r.Version
	}
	return m
}
