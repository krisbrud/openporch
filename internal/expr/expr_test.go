package expr

import (
	"errors"
	"testing"
)

type fakeState map[string]map[string]any

func (s fakeState) AliasOutputs(alias string) (map[string]any, bool) {
	m, ok := s[alias]
	return m, ok
}

type fakeVars map[string]string

func (v fakeVars) Var(n string) (string, bool) { x, ok := v[n]; return x, ok }

func TestResolve_context(t *testing.T) {
	t.Parallel()
	got, err := Resolve("hello ${context.env_id}!", Context{EnvID: "prod"}, nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	if got != "hello prod!" {
		t.Errorf("got %q", got)
	}
}

func TestResolve_resourcesAndShared(t *testing.T) {
	t.Parallel()
	st := fakeState{
		"workloads.api.db": {"url": "postgres://db", "host": "db.local"},
		"shared.bucket":    {"name": "my-bucket"},
	}
	got, err := Resolve(
		"DB=${resources.db.outputs.url} BUCKET=${shared.bucket.outputs.name}",
		Context{WorkloadName: "api"}, st, nil)
	if err != nil {
		t.Fatal(err)
	}
	if got != "DB=postgres://db BUCKET=my-bucket" {
		t.Errorf("got %q", got)
	}
}

func TestResolve_unresolvedLeavesPlaceholder(t *testing.T) {
	t.Parallel()
	st := fakeState{}
	got, err := Resolve("DB=${resources.db.outputs.url}", Context{WorkloadName: "api"}, st, nil)
	if !errors.Is(err, ErrUnresolved) {
		t.Fatalf("want ErrUnresolved, got %v", err)
	}
	if got != "DB=${resources.db.outputs.url}" {
		t.Errorf("expected placeholder retained, got %q", got)
	}
}

func TestResolve_var(t *testing.T) {
	t.Parallel()
	got, err := Resolve("X=${var.MY_KEY}", Context{}, nil, fakeVars{"MY_KEY": "v"})
	if err != nil {
		t.Fatal(err)
	}
	if got != "X=v" {
		t.Errorf("got %q", got)
	}
}

func TestResolveAny_walksMaps(t *testing.T) {
	t.Parallel()
	in := map[string]any{
		"a": "${context.env_id}",
		"b": []any{"${context.project_id}", 42},
		"c": map[string]any{"d": "${context.env_type_id}"},
	}
	out, err := ResolveAny(in, Context{EnvID: "e", ProjectID: "p", EnvTypeID: "t"}, nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	m := out.(map[string]any)
	if m["a"] != "e" || m["c"].(map[string]any)["d"] != "t" {
		t.Errorf("got %#v", m)
	}
	arr := m["b"].([]any)
	if arr[0] != "p" || arr[1] != 42 {
		t.Errorf("got %#v", arr)
	}
}
