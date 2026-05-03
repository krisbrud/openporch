package tofu

import (
	"path/filepath"
	"testing"
)

func TestResolveSource_inline(t *testing.T) {
	t.Parallel()
	got, err := ResolveSource("inline", "/anywhere")
	if err != nil {
		t.Fatal(err)
	}
	if got != "./module" {
		t.Fatalf("got %q, want ./module", got)
	}
}

func TestResolveSource_localRelative(t *testing.T) {
	t.Parallel()
	root := t.TempDir()
	got, err := ResolveSource("./modules/postgres", root)
	if err != nil {
		t.Fatal(err)
	}
	want := filepath.Join(root, "modules", "postgres")
	if got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}

func TestResolveSource_localParent(t *testing.T) {
	t.Parallel()
	root := t.TempDir()
	got, err := ResolveSource("../shared/postgres", root)
	if err != nil {
		t.Fatal(err)
	}
	want, _ := filepath.Abs(filepath.Join(root, "../shared/postgres"))
	if got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}

func TestResolveSource_remoteVerbatim(t *testing.T) {
	t.Parallel()
	cases := []string{
		"git::https://example.com/foo.git//modules/postgres",
		"registry.terraform.io/hashicorp/consul/aws",
		"github.com/org/repo//modules/postgres",
		"https://example.com/module.zip",
		"git@github.com:org/repo.git",
	}
	for _, raw := range cases {
		got, err := ResolveSource(raw, "/somewhere")
		if err != nil {
			t.Fatalf("%q: %v", raw, err)
		}
		if got != raw {
			t.Fatalf("%q: got %q, want verbatim", raw, got)
		}
	}
}

func TestResolveSource_bareAsLocal(t *testing.T) {
	t.Parallel()
	root := t.TempDir()
	got, err := ResolveSource("modules/postgres", root)
	if err != nil {
		t.Fatal(err)
	}
	want := filepath.Join(root, "modules", "postgres")
	if got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}

func TestIsLocalSource(t *testing.T) {
	t.Parallel()
	cases := map[string]bool{
		"./foo":                     true,
		"../foo":                    true,
		"/abs/foo":                  true,
		"foo/bar":                   true,
		"inline":                    false,
		"":                          false,
		"git::https://x/y.git":      false,
		"github.com/org/repo":       false,
		"bitbucket.org/org/repo":    false,
		"registry.terraform.io/x/y": false,
		"app.terraform.io/x/y":      false,
		"https://example.com/x.zip": false,
		"http://example.com/x.zip":  false,
		"git@github.com:org/repo":   false,
	}
	for in, want := range cases {
		if got := IsLocalSource(in); got != want {
			t.Errorf("IsLocalSource(%q) = %v, want %v", in, got, want)
		}
	}
}
